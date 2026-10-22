use std::env;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use crate::database::initialize_database;

use super::*;

fn unique_root(tag: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    env::temp_dir().join(format!("cubic-shared-files-{tag}-{stamp}"))
}

struct Fixture {
    root: PathBuf,
    paths: LauncherPaths,
    connection: Connection,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let root = unique_root(tag);
        let paths = LauncherPaths::new(&root);
        paths
            .create_required_directories()
            .expect("the test root should be creatable");
        initialize_database(paths.database_path()).expect("the test database should initialize");
        let connection =
            Connection::open(paths.database_path()).expect("the test database should open");

        Self {
            root,
            paths,
            connection,
        }
    }

    /// A mod list exists when its folder has a `rules.json`: the shared copy
    /// is only written into one that does (`dati-5`).
    fn ensure_modlist(&self, modlist: &str) {
        let dir = self.paths.modlists_dir().join(modlist);
        fs::create_dir_all(&dir).expect("the mod list directory should be creatable");
        let rules = dir.join("rules.json");
        if !rules.exists() {
            fs::write(rules, b"{}").expect("rules.json should be writable");
        }
    }

    fn instance_root(&self, modlist: &str, instance: &str) -> PathBuf {
        self.ensure_modlist(modlist);
        let dir = self
            .paths
            .modlists_dir()
            .join(modlist)
            .join("instances")
            .join(instance);
        fs::create_dir_all(&dir).expect("the instance directory should be creatable");
        dir
    }

    fn write_instance_file(&self, modlist: &str, instance: &str, file: SharedFile, body: &[u8]) {
        let root = self.instance_root(modlist, instance);
        fs::write(instance_path(&root, file), body).expect("the instance file should be writable");
    }

    fn canonical(&self, modlist: &str, file: SharedFile) -> PathBuf {
        canonical_path(&self.paths, modlist, file).expect("the canonical path should resolve")
    }

    fn write_canonical(&self, modlist: &str, file: SharedFile, body: &[u8]) {
        self.ensure_modlist(modlist);
        let path = self.canonical(modlist, file);
        fs::create_dir_all(path.parent().expect("a canonical path has a parent"))
            .expect("the mod list directory should be creatable");
        fs::write(path, body).expect("the canonical copy should be writable");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn action_for(statuses: &[SharedFileStatus], file: SharedFile) -> &SharedFileAction {
    &statuses
        .iter()
        .find(|status| status.file == file)
        .expect("every shared file must report an action")
        .action
}

// ---------------------------------------------------------------------------
// La copia dentro e la copia fuori
// ---------------------------------------------------------------------------

#[test]
fn the_first_instance_with_a_server_list_lends_it_to_the_mod_list() {
    // Il caso vero: un'istanza ha un `servers.dat` scritto dal gioco, la
    // modlist non ha ancora niente. La copia canonica nasce da lì, e l'istanza
    // non viene toccata.
    let fixture = Fixture::new("adopt");
    let forge = fixture.instance_root("Drehmal", "1.20.1-forge");
    fixture.write_instance_file(
        "Drehmal",
        "1.20.1-forge",
        SharedFile::Servers,
        b"lista-vera",
    );

    let statuses = copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge, None);

    assert_eq!(
        action_for(&statuses, SharedFile::Servers),
        &SharedFileAction::Copied { bytes: 10 }
    );
    assert_eq!(
        fs::read(fixture.canonical("Drehmal", SharedFile::Servers)).unwrap(),
        b"lista-vera"
    );
    assert_eq!(
        fs::read(instance_path(&forge, SharedFile::Servers)).unwrap(),
        b"lista-vera",
        "l'istanza che presta la lista non deve perderla"
    );

    // Gli altri due file non esistono da nessuna parte: non si inventa niente.
    assert_eq!(
        action_for(&statuses, SharedFile::Hotbar),
        &SharedFileAction::NothingToCopy
    );
    assert!(!fixture.canonical("Drehmal", SharedFile::Hotbar).exists());
}

#[test]
fn a_second_instance_receives_the_list_it_never_had() {
    // È la misura sui suoi dati: `1.20.1-forge` ha un `servers.dat`,
    // `1.20.1-fabric` no.
    let fixture = Fixture::new("second");
    let forge = fixture.instance_root("Drehmal", "1.20.1-forge");
    let fabric = fixture.instance_root("Drehmal", "1.20.1-fabric");
    fixture.write_instance_file(
        "Drehmal",
        "1.20.1-forge",
        SharedFile::Servers,
        b"lista-vera",
    );

    copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge, None);
    let statuses = copy_into_instance(
        &fixture.paths,
        &fixture.connection,
        "Drehmal",
        &fabric,
        None,
    );

    assert_eq!(
        action_for(&statuses, SharedFile::Servers),
        &SharedFileAction::Copied { bytes: 10 }
    );
    assert_eq!(
        fs::read(instance_path(&fabric, SharedFile::Servers)).unwrap(),
        b"lista-vera"
    );
}

#[test]
fn an_instance_that_changed_nothing_does_not_rewrite_the_canonical_copy() {
    let fixture = Fixture::new("identical");
    let forge = fixture.instance_root("Drehmal", "1.20.1-forge");
    fixture.write_instance_file(
        "Drehmal",
        "1.20.1-forge",
        SharedFile::Servers,
        b"lista-vera",
    );
    copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge, None);

    let statuses = copy_back_from_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge);

    assert_eq!(
        action_for(&statuses, SharedFile::Servers),
        &SharedFileAction::AlreadyIdentical
    );
}

#[test]
fn the_last_instance_to_exit_overwrites_the_canonical_copy() {
    // D78, scritto come test perché è un limite dichiarato e non un difetto da
    // scoprire per caso: se la copia canonica è cambiata mentre l'istanza
    // giocava, l'uscita la riscrive con quello che ha in mano.
    let fixture = Fixture::new("last-wins");
    let forge = fixture.instance_root("Drehmal", "1.20.1-forge");
    fixture.write_instance_file(
        "Drehmal",
        "1.20.1-forge",
        SharedFile::Servers,
        b"lista-di-forge",
    );
    fixture.write_canonical(
        "Drehmal",
        SharedFile::Servers,
        b"lista-aggiunta-dall-altra-istanza",
    );

    let statuses = copy_back_from_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge);

    assert_eq!(
        action_for(&statuses, SharedFile::Servers),
        &SharedFileAction::Copied { bytes: 14 }
    );
    assert_eq!(
        fs::read(fixture.canonical("Drehmal", SharedFile::Servers)).unwrap(),
        b"lista-di-forge",
        "l'ultima che esce vince: il limite di D78"
    );
}

#[test]
fn an_instance_without_the_files_leaves_the_canonical_copy_alone() {
    let fixture = Fixture::new("empty-instance");
    let neoforge = fixture.instance_root("Drehmal", "1.20.1-neoforge");
    fixture.write_canonical("Drehmal", SharedFile::Servers, b"lista-vera");

    let statuses =
        copy_back_from_instance(&fixture.paths, &fixture.connection, "Drehmal", &neoforge);

    assert_eq!(
        action_for(&statuses, SharedFile::Servers),
        &SharedFileAction::NothingToCopy
    );
    assert_eq!(
        fs::read(fixture.canonical("Drehmal", SharedFile::Servers)).unwrap(),
        b"lista-vera"
    );
}

#[test]
fn the_list_an_instance_had_of_its_own_survives_as_a_backup() {
    // Il caso di aggiornamento, e non è teorico: chi ha due istanze con due
    // liste diverse scritte prima di E2 ne vede sostituire una, e a decidere
    // quale è l'ordine di lancio. D80 non cambia quale vince — quella parte è
    // D78 e resta — ma fa in modo che la perdente **resti su disco**, accanto
    // al file, dove l'utente può ritrovarla.
    let fixture = Fixture::new("overwrite");
    let forge = fixture.instance_root("Drehmal", "1.20.1-forge");
    let fabric = fixture.instance_root("Drehmal", "1.20.1-fabric");
    fixture.write_instance_file(
        "Drehmal",
        "1.20.1-forge",
        SharedFile::Servers,
        b"lista-di-forge",
    );
    fixture.write_instance_file(
        "Drehmal",
        "1.20.1-fabric",
        SharedFile::Servers,
        b"lista-di-fabric-mai-condivisa",
    );

    copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge, None);
    let statuses = copy_into_instance(
        &fixture.paths,
        &fixture.connection,
        "Drehmal",
        &fabric,
        None,
    );

    assert_eq!(
        action_for(&statuses, SharedFile::Servers),
        &SharedFileAction::CopiedAfterBackup {
            bytes: 14,
            backup: "servers.dat.bak".to_string()
        }
    );
    assert_eq!(
        fs::read(instance_path(&fabric, SharedFile::Servers)).unwrap(),
        b"lista-di-forge",
        "l'ultima parola resta alla copia canonica: limite dichiarato di D78"
    );
    assert_eq!(
        fs::read(backup_path(&instance_path(&fabric, SharedFile::Servers))).unwrap(),
        b"lista-di-fabric-mai-condivisa",
        "ma la lista che fabric aveva di suo è ancora lì (D80)"
    );
}

#[test]
fn the_backup_is_written_once_and_never_again() {
    // La seconda volta il file che si sovrascrive è quello che abbiamo scritto
    // noi: risalvarlo cancellerebbe l'unica cosa che vale la pena tenere.
    let fixture = Fixture::new("backup-once");
    let fabric = fixture.instance_root("Drehmal", "1.20.1-fabric");
    fixture.write_instance_file(
        "Drehmal",
        "1.20.1-fabric",
        SharedFile::Servers,
        b"lista-originale-di-fabric",
    );
    fixture.write_canonical("Drehmal", SharedFile::Servers, b"prima-lista-condivisa");

    copy_into_instance(
        &fixture.paths,
        &fixture.connection,
        "Drehmal",
        &fabric,
        None,
    );
    fixture.write_canonical("Drehmal", SharedFile::Servers, b"seconda-lista-condivisa");
    let statuses = copy_into_instance(
        &fixture.paths,
        &fixture.connection,
        "Drehmal",
        &fabric,
        None,
    );

    assert_eq!(
        action_for(&statuses, SharedFile::Servers),
        &SharedFileAction::Copied { bytes: 23 },
        "il secondo passaggio non deve creare un altro .bak"
    );
    assert_eq!(
        fs::read(backup_path(&instance_path(&fabric, SharedFile::Servers))).unwrap(),
        b"lista-originale-di-fabric",
        "il .bak resta la fotografia di prima della condivisione"
    );
}

#[test]
fn an_identical_file_never_leaves_a_backup() {
    let fixture = Fixture::new("backup-identical");
    let fabric = fixture.instance_root("Drehmal", "1.20.1-fabric");
    fixture.write_instance_file(
        "Drehmal",
        "1.20.1-fabric",
        SharedFile::Servers,
        b"stessa-lista",
    );
    fixture.write_canonical("Drehmal", SharedFile::Servers, b"stessa-lista");

    let statuses = copy_into_instance(
        &fixture.paths,
        &fixture.connection,
        "Drehmal",
        &fabric,
        None,
    );

    assert_eq!(
        action_for(&statuses, SharedFile::Servers),
        &SharedFileAction::AlreadyIdentical
    );
    assert!(!backup_path(&instance_path(&fabric, SharedFile::Servers)).exists());
}

#[test]
fn all_three_files_travel_together() {
    let fixture = Fixture::new("all-three");
    let forge = fixture.instance_root("Drehmal", "1.20.1-forge");
    let fabric = fixture.instance_root("Drehmal", "1.20.1-fabric");
    for (file, body) in [
        (SharedFile::Servers, b"srv".as_slice()),
        (SharedFile::Hotbar, b"hot".as_slice()),
        (SharedFile::CommandHistory, b"/login x\n".as_slice()),
    ] {
        fixture.write_instance_file("Drehmal", "1.20.1-forge", file, body);
    }

    copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge, None);
    copy_into_instance(
        &fixture.paths,
        &fixture.connection,
        "Drehmal",
        &fabric,
        None,
    );

    assert_eq!(
        fs::read(instance_path(&fabric, SharedFile::Hotbar)).unwrap(),
        b"hot"
    );
    assert_eq!(
        fs::read(instance_path(&fabric, SharedFile::CommandHistory)).unwrap(),
        b"/login x\n"
    );
}

#[test]
fn a_mod_list_name_that_escapes_its_directory_is_refused() {
    let fixture = Fixture::new("escape");
    let forge = fixture.instance_root("Drehmal", "1.20.1-forge");

    let statuses = copy_into_instance(&fixture.paths, &fixture.connection, "../evil", &forge, None);

    assert!(matches!(
        action_for(&statuses, SharedFile::Servers),
        SharedFileAction::Failed { .. }
    ));
}

// ---------------------------------------------------------------------------
// Il collegamento fra modlist
// ---------------------------------------------------------------------------

#[test]
fn linking_is_symmetric_and_both_sides_see_the_same_list() {
    let fixture = Fixture::new("link");
    let drehmal_instance = fixture.instance_root("Drehmal", "1.20.1-forge");
    let test2_instance = fixture.instance_root("test2", "26.3-fabric");
    fixture.write_canonical("test2", SharedFile::Servers, b"lista-di-test2");

    link_modlists(&fixture.connection, "Drehmal", "test2").expect("linking should persist");

    // Drehmal adotta la lista di test2…
    copy_into_instance(
        &fixture.paths,
        &fixture.connection,
        "Drehmal",
        &drehmal_instance,
        None,
    );
    assert_eq!(
        fs::read(instance_path(&drehmal_instance, SharedFile::Servers)).unwrap(),
        b"lista-di-test2"
    );

    // …e quello che Drehmal scrive lo vede test2: è la stessa lista, non due.
    fs::write(
        instance_path(&drehmal_instance, SharedFile::Servers),
        b"server-aggiunto-da-drehmal",
    )
    .expect("the instance file should be writable");
    copy_back_from_instance(
        &fixture.paths,
        &fixture.connection,
        "Drehmal",
        &drehmal_instance,
    );
    copy_into_instance(
        &fixture.paths,
        &fixture.connection,
        "test2",
        &test2_instance,
        None,
    );

    assert_eq!(
        fs::read(instance_path(&test2_instance, SharedFile::Servers)).unwrap(),
        b"server-aggiunto-da-drehmal"
    );
    assert!(
        !fixture.canonical("Drehmal", SharedFile::Servers).exists(),
        "i byte restano in un posto solo, quello del gruppo"
    );
}

#[test]
fn a_third_mod_list_joins_the_same_group_instead_of_starting_a_chain() {
    let fixture = Fixture::new("transitive");
    link_modlists(&fixture.connection, "Drehmal", "test2").expect("linking should persist");
    let groups =
        link_modlists(&fixture.connection, "terza", "Drehmal").expect("linking should persist");

    assert_eq!(groups.len(), 1, "un insieme, non una catena");
    assert_eq!(
        canonical_modlist(&fixture.connection, "terza").unwrap(),
        "test2"
    );
    assert_eq!(
        canonical_modlist(&fixture.connection, "Drehmal").unwrap(),
        "test2"
    );
}

#[test]
fn linking_the_same_pair_twice_does_not_move_the_canonical_copy() {
    let fixture = Fixture::new("idempotent");
    link_modlists(&fixture.connection, "Drehmal", "test2").expect("linking should persist");
    link_modlists(&fixture.connection, "test2", "Drehmal").expect("linking should persist");

    assert_eq!(
        canonical_modlist(&fixture.connection, "Drehmal").unwrap(),
        "test2",
        "ricollegare non deve spostare i byte"
    );
}

#[test]
fn an_unlinked_mod_list_sees_its_own_instances_again() {
    let fixture = Fixture::new("default-scope");
    assert_eq!(
        canonical_modlist(&fixture.connection, "Drehmal").unwrap(),
        "Drehmal",
        "senza collegamenti ogni modlist è il suo gruppo (D75)"
    );
}

#[test]
fn leaving_a_group_leaves_a_copy_on_both_sides() {
    let fixture = Fixture::new("unlink");
    fixture.write_canonical("test2", SharedFile::Servers, b"lista-condivisa");
    link_modlists(&fixture.connection, "Drehmal", "test2").expect("linking should persist");

    let groups = unlink_modlist(&fixture.paths, &fixture.connection, "Drehmal")
        .expect("unlinking should persist");

    assert!(groups.is_empty());
    assert_eq!(
        fs::read(fixture.canonical("Drehmal", SharedFile::Servers)).unwrap(),
        b"lista-condivisa",
        "chi esce si porta via quello che stava vedendo"
    );
    assert_eq!(
        fs::read(fixture.canonical("test2", SharedFile::Servers)).unwrap(),
        b"lista-condivisa",
        "e il gruppo che resta non perde niente"
    );
}

#[test]
fn the_group_survives_the_departure_of_the_mod_list_that_held_the_files() {
    let fixture = Fixture::new("unlink-canonical");
    fixture.write_canonical("test2", SharedFile::Servers, b"lista-condivisa");
    link_modlists(&fixture.connection, "Drehmal", "test2").expect("linking should persist");
    link_modlists(&fixture.connection, "terza", "test2").expect("linking should persist");

    let groups = unlink_modlist(&fixture.paths, &fixture.connection, "test2")
        .expect("unlinking should persist");

    assert_eq!(groups.len(), 1);
    assert_eq!(
        canonical_modlist(&fixture.connection, "terza").unwrap(),
        "Drehmal"
    );
    assert_eq!(
        fs::read(fixture.canonical("Drehmal", SharedFile::Servers)).unwrap(),
        b"lista-condivisa",
        "i byte devono seguire chi tiene il gruppo"
    );
}

#[test]
fn a_mod_list_cannot_be_linked_to_itself() {
    let fixture = Fixture::new("self-link");
    assert!(link_modlists(&fixture.connection, "Drehmal", "Drehmal").is_err());
}

#[test]
fn an_unreadable_groups_row_means_no_links_instead_of_a_failed_launch() {
    let fixture = Fixture::new("broken-row");
    fixture
        .connection
        .execute(
            "INSERT INTO global_settings (key, value) VALUES (?1, ?2)",
            [SHARED_FILE_GROUPS_KEY, "{ non è json }"],
        )
        .expect("the broken row should write");

    assert!(load_groups(&fixture.connection).unwrap().is_empty());
    assert_eq!(
        canonical_modlist(&fixture.connection, "Drehmal").unwrap(),
        "Drehmal"
    );
}

#[test]
fn saving_the_settings_form_leaves_the_links_alone() {
    // Stessa trappola dei mondi nascosti (D65): `global_settings` ha più di uno
    // scrittore.
    let fixture = Fixture::new("settings-save");
    link_modlists(&fixture.connection, "Drehmal", "test2").expect("linking should persist");

    crate::app_shell::save_global_settings(
        &fixture.connection,
        &crate::app_shell::ShellGlobalSettingsInput {
            min_ram_mb: 2048,
            max_ram_mb: 4096,
            custom_jvm_args: String::new(),
            profiler_enabled: false,
            update_notifications_enabled: true,
            update_notifications_resource_packs: true,
            update_notifications_data_packs: true,
            update_notifications_shaders: true,
            wrapper_command: String::new(),
            java_path_override: String::new(),
        },
    )
    .expect("saving the settings form should succeed");

    assert_eq!(
        canonical_modlist(&fixture.connection, "Drehmal").unwrap(),
        "test2"
    );
}

// ---------------------------------------------------------------------------
// L'export
// ---------------------------------------------------------------------------

#[test]
fn only_the_command_history_is_kept_out_of_an_archive() {
    assert!(is_excluded_from_export(COMMAND_HISTORY_FILENAME));
    assert!(!is_excluded_from_export(SERVERS_FILENAME));
    assert!(!is_excluded_from_export(HOTBAR_FILENAME));
    assert!(!is_excluded_from_export("options.txt"));
}

// ---------------------------------------------------------------------------
// Il guardiano delle hotbar (D79)
// ---------------------------------------------------------------------------

/// Un `hotbar.nbt` minimo ma vero: NBT **non compresso**, con la `DataVersion`
/// che `NbtUtils.addCurrentDataVersion` mette in testa. È l'unico campo che il
/// guardiano guarda.
fn hotbar_bytes(data_version: i32, marker: &str) -> Vec<u8> {
    #[derive(serde::Serialize)]
    struct Hotbar<'a> {
        #[serde(rename = "DataVersion")]
        data_version: i32,
        marker: &'a str,
    }
    fastnbt::to_bytes(&Hotbar {
        data_version,
        marker,
    })
    .expect("the test hotbar should serialize")
}

#[test]
fn hotbars_never_travel_down_to_an_older_game() {
    // 5023 è la DataVersion di 26.3, 3465 quella di 1.20.1: la copia canonica
    // è in formato `components` e l'istanza la leggerebbe come stack vuoti.
    let fixture = Fixture::new("hotbar-down");
    let old = fixture.instance_root("Drehmal", "1.20.1-forge");
    fixture.write_canonical("Drehmal", SharedFile::Hotbar, &hotbar_bytes(5023, "nuove"));

    let statuses = copy_into_instance(
        &fixture.paths,
        &fixture.connection,
        "Drehmal",
        &old,
        Some(3465),
    );

    let SharedFileAction::Refused { reason } = action_for(&statuses, SharedFile::Hotbar) else {
        panic!("copiare hotbar 5023 in un'istanza 3465 deve essere rifiutato");
    };
    assert!(
        reason.contains("5023") && reason.contains("3465"),
        "{reason}"
    );
    assert!(
        !instance_path(&old, SharedFile::Hotbar).exists(),
        "rifiutare vuol dire non scrivere niente"
    );
}

#[test]
fn hotbars_travel_up_to_a_newer_game_because_the_datafixer_handles_that() {
    let fixture = Fixture::new("hotbar-up");
    let new = fixture.instance_root("Drehmal", "1.21.1-fabric");
    fixture.write_canonical(
        "Drehmal",
        SharedFile::Hotbar,
        &hotbar_bytes(3465, "vecchie"),
    );

    let statuses = copy_into_instance(
        &fixture.paths,
        &fixture.connection,
        "Drehmal",
        &new,
        Some(5023),
    );

    assert!(
        matches!(
            action_for(&statuses, SharedFile::Hotbar),
            SharedFileAction::Copied { .. }
        ),
        "all'insù il DataFixer fa il suo mestiere: {:?}",
        action_for(&statuses, SharedFile::Hotbar)
    );
}

#[test]
fn an_old_instance_cannot_write_its_hotbars_over_newer_shared_ones() {
    let fixture = Fixture::new("hotbar-back");
    let old = fixture.instance_root("Drehmal", "1.20.1-forge");
    fixture.write_instance_file(
        "Drehmal",
        "1.20.1-forge",
        SharedFile::Hotbar,
        &hotbar_bytes(3465, "vecchie"),
    );
    fixture.write_canonical("Drehmal", SharedFile::Hotbar, &hotbar_bytes(5023, "nuove"));

    let statuses = copy_back_from_instance(&fixture.paths, &fixture.connection, "Drehmal", &old);

    assert!(
        matches!(
            action_for(&statuses, SharedFile::Hotbar),
            SharedFileAction::Refused { .. }
        ),
        "{:?}",
        action_for(&statuses, SharedFile::Hotbar)
    );
    assert_eq!(
        fs::read(fixture.canonical("Drehmal", SharedFile::Hotbar)).unwrap(),
        hotbar_bytes(5023, "nuove")
    );
}

#[test]
fn a_newer_instance_can_still_write_its_hotbars_back() {
    // La simmetria è ingannevole e il verso sbagliato congelerebbe la copia
    // canonica per sempre: qui la scrittura deve passare.
    let fixture = Fixture::new("hotbar-back-up");
    let new = fixture.instance_root("Drehmal", "1.21.1-fabric");
    fixture.write_instance_file(
        "Drehmal",
        "1.21.1-fabric",
        SharedFile::Hotbar,
        &hotbar_bytes(5023, "nuove"),
    );
    fixture.write_canonical(
        "Drehmal",
        SharedFile::Hotbar,
        &hotbar_bytes(3465, "vecchie"),
    );

    let statuses = copy_back_from_instance(&fixture.paths, &fixture.connection, "Drehmal", &new);

    assert!(
        matches!(
            action_for(&statuses, SharedFile::Hotbar),
            SharedFileAction::Copied { .. }
        ),
        "{:?}",
        action_for(&statuses, SharedFile::Hotbar)
    );
    assert_eq!(
        fs::read(fixture.canonical("Drehmal", SharedFile::Hotbar)).unwrap(),
        hotbar_bytes(5023, "nuove")
    );
}

#[test]
fn the_guard_only_looks_at_the_hotbars() {
    // Il `servers.dat` non ha nessuna DataVersion da rispettare: il formato
    // non è mai cambiato fra le versioni che ci interessano.
    let fixture = Fixture::new("hotbar-guard-scope");
    let old = fixture.instance_root("Drehmal", "1.20.1-forge");
    fixture.write_canonical("Drehmal", SharedFile::Servers, b"lista");

    let statuses = copy_into_instance(
        &fixture.paths,
        &fixture.connection,
        "Drehmal",
        &old,
        Some(3465),
    );

    assert_eq!(
        action_for(&statuses, SharedFile::Servers),
        &SharedFileAction::Copied { bytes: 5 }
    );
}

// ---------------------------------------------------------------------------
// Gli interruttori e la vista della GUI
// ---------------------------------------------------------------------------

#[test]
fn every_file_is_shared_until_someone_says_otherwise() {
    let fixture = Fixture::new("toggles-default");
    for file in SharedFile::ALL {
        assert!(shared_file_enabled(&fixture.connection, "Drehmal", file).unwrap());
    }
}

#[test]
fn a_file_switched_off_is_neither_copied_in_nor_out() {
    let fixture = Fixture::new("toggles-off");
    let forge = fixture.instance_root("Drehmal", "1.20.1-forge");
    fixture.write_canonical("Drehmal", SharedFile::CommandHistory, b"/login segreta\n");
    set_shared_file_enabled(
        &fixture.connection,
        "Drehmal",
        SharedFile::CommandHistory,
        false,
    )
    .expect("the switch should persist");

    let statuses = copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge, None);

    assert_eq!(
        action_for(&statuses, SharedFile::CommandHistory),
        &SharedFileAction::Disabled
    );
    assert!(!instance_path(&forge, SharedFile::CommandHistory).exists());

    // E l'altro verso: il file dell'istanza non finisce nella modlist.
    fixture.write_instance_file(
        "Drehmal",
        "1.20.1-forge",
        SharedFile::CommandHistory,
        b"/login altra\n",
    );
    let back = copy_back_from_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge);
    assert_eq!(
        action_for(&back, SharedFile::CommandHistory),
        &SharedFileAction::Disabled
    );
    assert_eq!(
        fs::read(fixture.canonical("Drehmal", SharedFile::CommandHistory)).unwrap(),
        b"/login segreta\n"
    );
}

#[test]
fn a_switch_belongs_to_one_mod_list_and_one_file() {
    let fixture = Fixture::new("toggles-scope");
    set_shared_file_enabled(&fixture.connection, "Drehmal", SharedFile::Hotbar, false)
        .expect("the switch should persist");

    assert!(!shared_file_enabled(&fixture.connection, "Drehmal", SharedFile::Hotbar).unwrap());
    assert!(shared_file_enabled(&fixture.connection, "Drehmal", SharedFile::Servers).unwrap());
    assert!(shared_file_enabled(&fixture.connection, "test2", SharedFile::Hotbar).unwrap());
}

#[test]
fn the_view_says_who_holds_the_files_and_who_can_still_be_linked() {
    let fixture = Fixture::new("view");
    for name in ["Drehmal", "test2", "terza"] {
        let dir = fixture.paths.modlists_dir().join(name);
        fs::create_dir_all(&dir).expect("the mod list directory should be creatable");
        fs::write(dir.join("rules.json"), b"{}").expect("rules.json should be writable");
    }

    let before = shared_files_view(&fixture.paths, &fixture.connection, "Drehmal")
        .expect("the view should build");
    assert!(before.group.is_empty(), "senza collegamenti non c'è gruppo");
    assert_eq!(before.canonical, "Drehmal");
    assert_eq!(
        before.linkable,
        vec!["terza".to_string(), "test2".to_string()]
    );
    assert_eq!(before.files.len(), 3);
    assert!(before.files.iter().all(|toggle| toggle.enabled));

    link_modlists(&fixture.connection, "Drehmal", "test2").expect("linking should persist");
    let after = shared_files_view(&fixture.paths, &fixture.connection, "Drehmal")
        .expect("the view should build");

    assert_eq!(after.canonical, "test2", "chi configura adotta");
    assert_eq!(
        after.group,
        vec!["test2".to_string(), "Drehmal".to_string()]
    );
    assert_eq!(after.linkable, vec!["terza".to_string()]);
}

#[test]
fn the_backup_never_leaves_in_an_archive() {
    assert!(is_excluded_from_export("servers.dat.bak"));
    assert!(is_excluded_from_export("hotbar.nbt.bak"));
    assert!(is_excluded_from_export("command_history.txt.bak"));
    assert!(!is_excluded_from_export("options.txt.bak"));
    assert!(!is_excluded_from_export("servers.dat"));
}

#[test]
fn hotbars_do_not_travel_when_nobody_can_say_which_version_reads_them() {
    // Non sapere è un no. È il caso normale di un'istanza al primo avvio il cui
    // jar non si è lasciato derivare: lasciar passare qui vorrebbe dire copiare
    // hotbar che potrebbero svuotarsi.
    let fixture = Fixture::new("hotbar-unknown");
    let unknown = fixture.instance_root("Drehmal", "1.20.1-forge");
    fixture.write_canonical("Drehmal", SharedFile::Hotbar, &hotbar_bytes(5023, "nuove"));

    let statuses = copy_into_instance(
        &fixture.paths,
        &fixture.connection,
        "Drehmal",
        &unknown,
        None,
    );

    let SharedFileAction::Refused { reason } = action_for(&statuses, SharedFile::Hotbar) else {
        panic!(
            "senza la DataVersion della destinazione la copia deve fermarsi, non passare: {:?}",
            action_for(&statuses, SharedFile::Hotbar)
        );
    };
    assert!(reason.contains("no way to tell"), "{reason}");
    assert!(!instance_path(&unknown, SharedFile::Hotbar).exists());
}

// ---------------------------------------------------------------------------
// La copia indietro mancata (D152, concorrenza-1)
// ---------------------------------------------------------------------------

/// Quello che il gioco fa a `servers.dat` quando l'utente aggiunge un server:
/// lo riscrive, con un `mtime` più nuovo di qualunque copia del launcher.
fn game_writes(path: &Path, body: &[u8]) {
    fs::write(path, body).expect("the game should be able to write its file");
    let later = SystemTime::now() + std::time::Duration::from_secs(5);
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|handle| handle.set_times(fs::FileTimes::new().set_modified(later)))
        .expect("the file's mtime should be settable");
}

#[test]
fn a_server_added_while_the_launcher_was_closed_comes_back_instead_of_vanishing() {
    // Il caso di `concorrenza-1`: Play, il launcher si chiude col gioco aperto,
    // l'utente aggiunge un server ed esce. La copia indietro non c'è stata. Al
    // lancio dopo la copia dentro scriveva sopra la lista vecchia, senza `.bak`.
    let fixture = Fixture::new("missed-copy-back");
    let forge = fixture.instance_root("Drehmal", "1.20.1-forge");
    fixture.write_canonical("Drehmal", SharedFile::Servers, b"lista-condivisa");
    copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge, None);

    game_writes(
        &instance_path(&forge, SharedFile::Servers),
        b"lista-con-il-server-nuovo",
    );
    // Nessuna copia indietro: il launcher era chiuso.
    let statuses = copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge, None);

    let SharedFileAction::BroughtBack { bytes, backup, .. } =
        action_for(&statuses, SharedFile::Servers)
    else {
        panic!(
            "il file cambiato dopo l'ultima copia deve tornare indietro: {:?}",
            action_for(&statuses, SharedFile::Servers)
        );
    };
    assert_eq!(*bytes, 25);
    assert_eq!(backup.as_deref(), Some("servers.dat.bak"));
    assert_eq!(
        fs::read(instance_path(&forge, SharedFile::Servers)).unwrap(),
        b"lista-con-il-server-nuovo",
        "il server aggiunto in gioco resta nell'istanza"
    );
    assert_eq!(
        fs::read(fixture.canonical("Drehmal", SharedFile::Servers)).unwrap(),
        b"lista-con-il-server-nuovo",
        "e arriva alla copia canonica"
    );
    assert_eq!(
        fs::read(backup_path(
            &fixture.canonical("Drehmal", SharedFile::Servers)
        ))
        .unwrap(),
        b"lista-condivisa",
        "la copia canonica sovrascritta resta su disco"
    );
    assert!(describe(&statuses, "copied into the instance").contains("brought back"));
}

#[test]
fn the_canonical_copy_another_instance_wrote_meanwhile_is_kept_as_a_backup() {
    let fixture = Fixture::new("missed-copy-back-race");
    let forge = fixture.instance_root("Drehmal", "1.20.1-forge");
    let fabric = fixture.instance_root("Drehmal", "1.20.1-fabric");
    fixture.write_canonical("Drehmal", SharedFile::Servers, b"lista-condivisa");
    copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge, None);
    game_writes(
        &instance_path(&forge, SharedFile::Servers),
        b"lista-di-forge",
    );

    // Intanto fabric gioca ed esce normalmente.
    copy_into_instance(
        &fixture.paths,
        &fixture.connection,
        "Drehmal",
        &fabric,
        None,
    );
    fs::write(
        instance_path(&fabric, SharedFile::Servers),
        b"lista-di-fabric",
    )
    .unwrap();
    copy_back_from_instance(&fixture.paths, &fixture.connection, "Drehmal", &fabric);

    copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge, None);

    assert_eq!(
        fs::read(fixture.canonical("Drehmal", SharedFile::Servers)).unwrap(),
        b"lista-di-forge"
    );
    assert_eq!(
        fs::read(backup_path(
            &fixture.canonical("Drehmal", SharedFile::Servers)
        ))
        .unwrap(),
        b"lista-di-fabric",
        "fra due liste cambiate tutte e due, quella che perde resta nel .bak"
    );
}

#[test]
fn an_instance_the_game_did_not_touch_still_receives_the_shared_list() {
    // L'altra metà: il file dell'istanza è quello che il launcher ha scritto,
    // quindi la copia canonica più nuova entra come prima.
    let fixture = Fixture::new("untouched-instance");
    let forge = fixture.instance_root("Drehmal", "1.20.1-forge");
    fixture.write_canonical("Drehmal", SharedFile::Servers, b"prima");
    copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge, None);
    fixture.write_canonical("Drehmal", SharedFile::Servers, b"seconda");

    let statuses = copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge, None);

    assert!(
        matches!(
            action_for(&statuses, SharedFile::Servers),
            SharedFileAction::Copied { bytes: 7 }
                | SharedFileAction::CopiedAfterBackup { bytes: 7, .. }
        ),
        "{:?}",
        action_for(&statuses, SharedFile::Servers)
    );
    assert_eq!(
        fs::read(instance_path(&forge, SharedFile::Servers)).unwrap(),
        b"seconda"
    );
}

#[test]
fn an_instance_from_before_the_sync_record_is_recognised_by_its_backup_and_date() {
    // Le istanze lanciate prima di D152 non hanno il registro. Un `.bak` dice
    // che il launcher ci ha già scritto sopra; un file più nuovo della copia
    // canonica dice che dopo l'ha scritto il gioco.
    let fixture = Fixture::new("legacy-instance");
    let forge = fixture.instance_root("Drehmal", "1.20.1-forge");
    fixture.write_canonical("Drehmal", SharedFile::Servers, b"lista-condivisa");
    let local = instance_path(&forge, SharedFile::Servers);
    fs::write(backup_path(&local), b"lista-di-prima-della-condivisione").unwrap();
    game_writes(&local, b"lista-giocata-dopo");

    let statuses = copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge, None);

    assert!(matches!(
        action_for(&statuses, SharedFile::Servers),
        SharedFileAction::BroughtBack { .. }
    ));
    assert_eq!(fs::read(&local).unwrap(), b"lista-giocata-dopo");
    assert_eq!(
        fs::read(backup_path(&local)).unwrap(),
        b"lista-di-prima-della-condivisione",
        "il .bak di D80 non si tocca"
    );
}

#[test]
fn an_older_instance_whose_hotbars_cannot_go_back_keeps_them() {
    // La guardia di D79 vale anche per la copia indietro mancata, e se ferma
    // il ritorno la copia dentro non deve passare sopra.
    let fixture = Fixture::new("missed-copy-back-hotbar");
    let old = fixture.instance_root("Drehmal", "1.20.1-forge");
    fixture.write_canonical("Drehmal", SharedFile::Hotbar, &hotbar_bytes(3465, "prima"));
    copy_into_instance(
        &fixture.paths,
        &fixture.connection,
        "Drehmal",
        &old,
        Some(3465),
    );
    game_writes(
        &instance_path(&old, SharedFile::Hotbar),
        &hotbar_bytes(3465, "giocate"),
    );
    fixture.write_canonical("Drehmal", SharedFile::Hotbar, &hotbar_bytes(5023, "nuove"));

    let statuses = copy_into_instance(
        &fixture.paths,
        &fixture.connection,
        "Drehmal",
        &old,
        Some(3465),
    );

    assert!(matches!(
        action_for(&statuses, SharedFile::Hotbar),
        SharedFileAction::Refused { .. }
    ));
    assert_eq!(
        fs::read(instance_path(&old, SharedFile::Hotbar)).unwrap(),
        hotbar_bytes(3465, "giocate")
    );
}

#[test]
fn the_copy_into_remembers_what_it_wrote() {
    let fixture = Fixture::new("sync-record");
    let forge = fixture.instance_root("Drehmal", "1.20.1-forge");
    fixture.write_canonical("Drehmal", SharedFile::Servers, b"lista");

    copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge, None);

    let record = load_sync_record(&forge).expect("the record should be written");
    assert_eq!(record.owner.as_deref(), Some("Drehmal"));
    assert_eq!(record.synced.get("servers"), Some(&fingerprint(b"lista")));
    assert!(sync_record_path(&forge).starts_with(forge.join(".cubic")));
}

// ---------------------------------------------------------------------------
// Il proprietario del lancio (condivisi-3)
// ---------------------------------------------------------------------------

#[test]
fn linking_while_the_game_runs_does_not_reverse_the_copy_back() {
    // Play su Drehmal; col gioco aperto, «Use test2's files». All'uscita la
    // lista del gioco andava in `mod-lists/test2/`, cioè sopra la lista che
    // l'utente aveva appena scelto di adottare.
    let fixture = Fixture::new("link-while-running");
    let forge = fixture.instance_root("Drehmal", "1.20.1-forge");
    fixture.write_canonical("Drehmal", SharedFile::Servers, b"lista-di-drehmal");
    fixture.write_canonical("test2", SharedFile::Servers, b"lista-di-test2");
    copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge, None);

    link_modlists(&fixture.connection, "Drehmal", "test2").expect("linking should persist");
    fs::write(instance_path(&forge, SharedFile::Servers), b"lista-giocata").unwrap();
    copy_back_from_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge);

    assert_eq!(
        fs::read(fixture.canonical("test2", SharedFile::Servers)).unwrap(),
        b"lista-di-test2",
        "la lista adottata non si tocca"
    );
    assert_eq!(
        fs::read(fixture.canonical("Drehmal", SharedFile::Servers)).unwrap(),
        b"lista-giocata",
        "la copia indietro va al proprietario del lancio"
    );

    // E al lancio dopo l'istanza riceve la lista di test2, come confermato.
    copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge, None);
    assert_eq!(
        fs::read(instance_path(&forge, SharedFile::Servers)).unwrap(),
        b"lista-di-test2"
    );
}

#[test]
fn a_recorded_owner_that_no_longer_exists_is_not_recreated() {
    let fixture = Fixture::new("stale-owner");
    let forge = fixture.instance_root("Drehmal", "1.20.1-forge");
    let record = SyncRecord {
        owner: Some("cancellata".to_string()),
        synced: BTreeMap::new(),
    };
    save_sync_record(&forge, &record).unwrap();
    fs::write(instance_path(&forge, SharedFile::Servers), b"lista").unwrap();

    copy_back_from_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge);

    assert!(!fixture.paths.modlists_dir().join("cancellata").exists());
    assert_eq!(
        fs::read(fixture.canonical("Drehmal", SharedFile::Servers)).unwrap(),
        b"lista"
    );
}

#[test]
fn linking_is_refused_while_a_game_of_the_group_runs() {
    let groups = vec![SharedGroup {
        members: vec!["test2".to_string(), "Drehmal".to_string()],
    }];

    assert!(link_refusal(&groups, &["Drehmal", "terza"], Some("Drehmal")).is_some());
    assert!(
        link_refusal(&groups, &["terza", "test2"], Some("Drehmal")).is_some(),
        "test2 è nel gruppo di Drehmal: cambiare test2 cambia la copia di Drehmal"
    );
    assert!(link_refusal(&groups, &["terza", "quarta"], Some("Drehmal")).is_none());
    assert!(link_refusal(&groups, &["Drehmal", "terza"], None).is_none());
}

// ---------------------------------------------------------------------------
// Errori che non si ingoiano più
// ---------------------------------------------------------------------------

#[test]
fn unreadable_switches_share_nothing_and_are_not_overwritten() {
    // Prima una riga illeggibile valeva "tutto acceso": riaccendeva
    // command_history.txt (D77) e il primo interruttore cancellava le scelte
    // delle altre modlist.
    let fixture = Fixture::new("switches-unreadable");
    let forge = fixture.instance_root("Drehmal", "1.20.1-forge");
    fixture.write_canonical("Drehmal", SharedFile::CommandHistory, b"/login segreta\n");
    fixture
        .connection
        .execute(
            "INSERT INTO global_settings (key, value) VALUES (?1, 'non json')",
            [SHARED_FILE_OPTIONS_KEY],
        )
        .unwrap();

    let statuses = copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge, None);
    assert!(matches!(
        action_for(&statuses, SharedFile::CommandHistory),
        SharedFileAction::Failed { .. }
    ));
    assert!(!instance_path(&forge, SharedFile::CommandHistory).exists());

    assert!(
        set_shared_file_enabled(&fixture.connection, "Drehmal", SharedFile::Servers, false)
            .is_err()
    );
    let stored: String = fixture
        .connection
        .query_row(
            "SELECT value FROM global_settings WHERE key = ?1",
            [SHARED_FILE_OPTIONS_KEY],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, "non json");
}

#[test]
fn an_instance_file_that_cannot_be_read_is_never_overwritten() {
    // Il `.bak` di D80 nasceva solo se la lettura riusciva: un file illeggibile
    // veniva sovrascritto senza copia. Qui il "file" è una cartella, il modo
    // più semplice di avere un percorso che c'è e non si legge.
    let fixture = Fixture::new("unreadable-target");
    let forge = fixture.instance_root("Drehmal", "1.20.1-forge");
    fixture.write_canonical("Drehmal", SharedFile::Servers, b"lista");
    fs::create_dir_all(instance_path(&forge, SharedFile::Servers)).unwrap();

    let statuses = copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge, None);

    assert!(matches!(
        action_for(&statuses, SharedFile::Servers),
        SharedFileAction::Failed { .. }
    ));
    assert!(instance_path(&forge, SharedFile::Servers).is_dir());
}

#[test]
fn an_unreadable_shared_hotbar_stops_the_write_back() {
    let fixture = Fixture::new("hotbar-canonical-unreadable");
    let old = fixture.instance_root("Drehmal", "1.20.1-forge");
    fixture.write_instance_file(
        "Drehmal",
        "1.20.1-forge",
        SharedFile::Hotbar,
        &hotbar_bytes(3465, "vecchie"),
    );
    fs::create_dir_all(fixture.canonical("Drehmal", SharedFile::Hotbar)).unwrap();

    let statuses = copy_back_from_instance(&fixture.paths, &fixture.connection, "Drehmal", &old);

    assert!(matches!(
        action_for(&statuses, SharedFile::Hotbar),
        SharedFileAction::Failed { .. }
    ));
}

#[test]
fn a_truncated_backup_is_written_again() {
    // Un `.bak` rimasto a metà da una scrittura interrotta: comincia come un
    // NBT e non finisce. "Una volta sola" non deve tenerlo così per sempre.
    let fixture = Fixture::new("truncated-backup");
    let fabric = fixture.instance_root("Drehmal", "1.20.1-fabric");
    let local = instance_path(&fabric, SharedFile::Servers);
    let own = hotbar_bytes(3465, "la-lista-di-fabric");
    fs::write(&local, &own).unwrap();
    fs::write(backup_path(&local), &own[..own.len() / 2]).unwrap();
    fixture.write_canonical("Drehmal", SharedFile::Servers, b"lista-condivisa");

    let statuses = copy_into_instance(
        &fixture.paths,
        &fixture.connection,
        "Drehmal",
        &fabric,
        None,
    );

    assert!(matches!(
        action_for(&statuses, SharedFile::Servers),
        SharedFileAction::CopiedAfterBackup { .. }
    ));
    assert_eq!(fs::read(backup_path(&local)).unwrap(), own);
}

#[test]
fn copies_leave_no_partial_file_behind() {
    let fixture = Fixture::new("no-partial");
    let forge = fixture.instance_root("Drehmal", "1.20.1-forge");
    fixture.write_canonical("Drehmal", SharedFile::Servers, b"lista");

    copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge, None);

    let leftovers: Vec<_> = fs::read_dir(&forge)
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".part"))
        .collect();
    assert!(leftovers.is_empty());
}

// ---------------------------------------------------------------------------
// La cancellazione di una modlist (`dati-5`, `r2-ciclo-vita-nome-1`)
// ---------------------------------------------------------------------------

#[test]
fn deleting_the_holder_hands_the_bytes_over_and_leaves_no_trace_in_the_groups() {
    let fixture = Fixture::new("delete-holder");
    fixture.write_canonical("A", SharedFile::Servers, b"lista-di-a");
    fixture.ensure_modlist("B");
    fixture.ensure_modlist("C");
    // B e C entrano nel gruppo di A: A tiene i byte.
    link_modlists(&fixture.connection, "B", "A").expect("linking should persist");
    link_modlists(&fixture.connection, "C", "A").expect("linking should persist");
    set_shared_file_enabled(&fixture.connection, "A", SharedFile::CommandHistory, false)
        .expect("the toggle should persist");

    hand_over_before_delete(&fixture.paths, &fixture.connection, "A")
        .expect("the hand-over should work");
    fs::remove_dir_all(fixture.paths.modlists_dir().join("A")).expect("A goes away");
    forget_modlist(&fixture.connection, "A").expect("forgetting should persist");

    assert_eq!(
        fs::read(fixture.canonical("B", SharedFile::Servers)).unwrap(),
        b"lista-di-a",
        "il nuovo portatore riceve la lista prima che A sparisca"
    );
    let groups = load_groups(&fixture.connection).unwrap();
    assert_eq!(
        groups,
        vec![SharedGroup {
            members: vec!["B".to_string(), "C".to_string()]
        }]
    );
    assert!(
        !load_file_options(&fixture.connection)
            .expect("the switches should be readable")
            .contains_key("A"),
        "gli interruttori di A non passano a una A nuova"
    );
}

#[test]
fn deleting_a_member_of_a_pair_dissolves_the_group_and_copies_nothing_into_it() {
    let fixture = Fixture::new("delete-member");
    fixture.write_canonical("A", SharedFile::Servers, b"lista-di-a");
    fixture.ensure_modlist("B");
    link_modlists(&fixture.connection, "B", "A").expect("linking should persist");

    hand_over_before_delete(&fixture.paths, &fixture.connection, "B")
        .expect("the hand-over should work");
    assert!(
        !fixture.canonical("B", SharedFile::Servers).exists(),
        "chi viene cancellato non riceve una copia"
    );
    forget_modlist(&fixture.connection, "B").expect("forgetting should persist");

    assert!(load_groups(&fixture.connection).unwrap().is_empty());
    assert_eq!(canonical_modlist(&fixture.connection, "A").unwrap(), "A");
}

#[test]
fn a_group_whose_holder_is_gone_does_not_bring_its_folder_back() {
    // Il caso di `dati-5` con un gruppo lasciato da una build vecchia: A è
    // stata cancellata, il gruppo la nomina ancora come portatrice. Il lancio
    // di B non deve ricreare `mod-lists/A/`.
    let fixture = Fixture::new("ghost");
    let b_instance = fixture.instance_root("B", "1.20.1-forge");
    fs::write(
        instance_path(&b_instance, SharedFile::Servers),
        b"lista-di-b",
    )
    .unwrap();
    link_modlists(&fixture.connection, "B", "A").expect("linking should persist");
    assert_eq!(canonical_modlist(&fixture.connection, "B").unwrap(), "A");

    let into = copy_into_instance(&fixture.paths, &fixture.connection, "B", &b_instance, None);
    let back = copy_back_from_instance(&fixture.paths, &fixture.connection, "B", &b_instance);

    for statuses in [&into, &back] {
        assert!(matches!(
            action_for(statuses, SharedFile::Servers),
            SharedFileAction::Failed { reason } if reason.contains("no longer exists")
        ));
    }
    assert!(
        !fixture.paths.modlists_dir().join("A").exists(),
        "nessuna cartella fantasma"
    );
}
