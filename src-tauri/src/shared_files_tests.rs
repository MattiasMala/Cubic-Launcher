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

    fn instance_root(&self, modlist: &str, instance: &str) -> PathBuf {
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
    fixture.write_instance_file("Drehmal", "1.20.1-forge", SharedFile::Servers, b"lista-vera");

    let statuses = copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge);

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
    fixture.write_instance_file("Drehmal", "1.20.1-forge", SharedFile::Servers, b"lista-vera");

    copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge);
    let statuses = copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &fabric);

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
    fixture.write_instance_file("Drehmal", "1.20.1-forge", SharedFile::Servers, b"lista-vera");
    copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge);

    let statuses =
        copy_back_from_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge);

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
    fixture.write_instance_file("Drehmal", "1.20.1-forge", SharedFile::Servers, b"lista-di-forge");
    fixture.write_canonical(
        "Drehmal",
        SharedFile::Servers,
        b"lista-aggiunta-dall-altra-istanza",
    );

    let statuses =
        copy_back_from_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge);

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
fn an_instance_with_its_own_list_loses_it_to_the_canonical_copy() {
    // Il caso di aggiornamento, e non è teorico: chi ha due istanze con due
    // liste diverse scritte prima di E2 ne perde una. La prima che viene
    // lanciata presta la sua lista alla modlist; la seconda, al suo primo
    // lancio, se la vede sostituire. **È l'ordine di lancio a decidere quale
    // delle due sopravvive**, e niente lo rende visibile all'utente.
    //
    // Il test fissa il comportamento perché è la conseguenza diretta della
    // copia semplice (D78): se un giorno si decide di tenere un `.bak` prima
    // di sovrascrivere, o di rifiutare la prima sovrascrittura divergente,
    // questo test deve fallire e far leggere questo commento.
    let fixture = Fixture::new("overwrite");
    let forge = fixture.instance_root("Drehmal", "1.20.1-forge");
    let fabric = fixture.instance_root("Drehmal", "1.20.1-fabric");
    fixture.write_instance_file("Drehmal", "1.20.1-forge", SharedFile::Servers, b"lista-di-forge");
    fixture.write_instance_file(
        "Drehmal",
        "1.20.1-fabric",
        SharedFile::Servers,
        b"lista-di-fabric-mai-condivisa",
    );

    copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge);
    let statuses = copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &fabric);

    assert_eq!(
        action_for(&statuses, SharedFile::Servers),
        &SharedFileAction::Copied { bytes: 14 }
    );
    assert_eq!(
        fs::read(instance_path(&fabric, SharedFile::Servers)).unwrap(),
        b"lista-di-forge",
        "la lista che fabric aveva di suo è persa: limite dichiarato di D78"
    );
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

    copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &forge);
    copy_into_instance(&fixture.paths, &fixture.connection, "Drehmal", &fabric);

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

    let statuses = copy_into_instance(&fixture.paths, &fixture.connection, "../evil", &forge);

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

