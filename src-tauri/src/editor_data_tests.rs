use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::rules::{ModList, ModSource, Rule};

use super::*;

fn unique_test_root() -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    env::temp_dir().join(format!("cubic-editor-test-{ts}"))
}

fn setup_modlist(root_dir: &Path, name: &str, rules: Vec<Rule>) {
    let modlist_dir = root_dir.join("mod-lists").join(name);
    fs::create_dir_all(&modlist_dir).unwrap();

    ModList {
        modlist_name: name.into(),
        author: "Author".into(),
        description: "".into(),
        rules,
    }
    .write_to_file(&modlist_dir.join("rules.json"))
    .unwrap();
}

fn simple_rule(mod_id: &str) -> Rule {
    Rule {
        mod_id: mod_id.into(),
        source: ModSource::Modrinth,
        enabled: true,
        exclude_if: vec![],
        requires: vec![],
        version_rules: vec![],
        custom_configs: vec![],
        alternatives: vec![],
    }
}

#[test]
fn load_editor_snapshot_returns_rows_and_incompatibilities() {
    let root = unique_test_root();
    setup_modlist(
        &root,
        "Test Pack",
        vec![
            Rule {
                mod_id: "sodium".into(),
                source: ModSource::Modrinth,
                enabled: true,
                exclude_if: vec![],
                requires: vec![],
                version_rules: vec![],
                custom_configs: vec![],
                alternatives: vec![simple_rule("rubidium")],
            },
            Rule {
                mod_id: "embeddium".into(),
                source: ModSource::Modrinth,
                enabled: true,
                exclude_if: vec!["sodium".into()],
                requires: vec![],
                version_rules: vec![],
                custom_configs: vec![],
                alternatives: vec![],
            },
        ],
    );

    let snapshot = load_editor_snapshot_from_root(&root, "Test Pack").unwrap();

    assert_eq!(snapshot.modlist_name, "Test Pack");
    assert_eq!(snapshot.rows.len(), 2);
    assert_eq!(snapshot.rows[0].mod_id, "sodium");
    assert_eq!(snapshot.rows[0].alternatives.len(), 1);
    assert_eq!(snapshot.rows[0].alternatives[0].mod_id, "rubidium");
    assert_eq!(snapshot.incompatibilities.len(), 1);
    assert_eq!(snapshot.incompatibilities[0].winner_id, "sodium");
    assert_eq!(snapshot.incompatibilities[0].loser_id, "embeddium");

    fs::remove_dir_all(&root).unwrap();
}

/// A local rule **and** a local alternative both get the icon of their jar: the
/// snapshot builds alternatives by recursion, and losing that recursion would
/// leave every fallback option with a placeholder while its parent shows an icon.
#[test]
fn local_rules_and_local_alternatives_both_get_their_jar_icon() {
    use std::io::Write;

    let root = unique_test_root();
    let mut top = simple_rule("top-local");
    top.source = ModSource::Local;
    let mut alt = simple_rule("alt-local");
    alt.source = ModSource::Local;
    top.alternatives = vec![alt];
    setup_modlist(&root, "Pack", vec![top]);

    let jars = root.join("mod-lists").join("Pack").join("local-jars");
    fs::create_dir_all(&jars).unwrap();
    for mod_id in ["top-local", "alt-local"] {
        let file = fs::File::create(jars.join(format!("{mod_id}.jar"))).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file("fabric.mod.json", zip::write::FileOptions::default())
            .unwrap();
        zip.write_all(br#"{"id":"m","icon":"icon.png"}"#).unwrap();
        zip.start_file("icon.png", zip::write::FileOptions::default())
            .unwrap();
        zip.write_all(b"\x89PNG\r\n\x1a\nbytes").unwrap();
        zip.finish().unwrap();
    }

    let snapshot = load_editor_snapshot_from_root(&root, "Pack").unwrap();
    let top_icon = snapshot.rows[0].icon_image.clone();
    let alt_icon = snapshot.rows[0].alternatives[0].icon_image.clone();

    fs::remove_dir_all(&root).unwrap();
    assert!(top_icon
        .unwrap_or_default()
        .starts_with("data:image/png;base64,"));
    assert!(
        alt_icon
            .unwrap_or_default()
            .starts_with("data:image/png;base64,"),
        "an alternative's icon is what the recursion is for"
    );
}

#[test]
fn load_modlist_rejects_traversal_name() {
    let root = unique_test_root();
    setup_modlist(&root, "../x", vec![simple_rule("sodium")]);

    let result = load_editor_snapshot_from_root(&root, "../x");

    fs::remove_dir_all(&root).unwrap();
    assert!(result.is_err());
}

#[test]
fn add_mod_rule_appends_rule() {
    let root = unique_test_root();
    setup_modlist(&root, "Pack", vec![simple_rule("sodium")]);

    add_mod_rule_from_root(
        &root,
        &AddModRuleInput {
            modlist_name: "Pack".into(),
            mod_id: "lithium".into(),
            source: "modrinth".into(),
            file_name: None,
        },
    )
    .unwrap();

    let snapshot = load_editor_snapshot_from_root(&root, "Pack").unwrap();
    assert_eq!(snapshot.rows.len(), 2);
    assert_eq!(snapshot.rows[1].mod_id, "lithium");

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn add_local_rule_rejects_traversal_mod_id() {
    let root = unique_test_root();
    setup_modlist(&root, "Pack", vec![]);
    let source_path = root.join("source.jar");
    fs::write(&source_path, b"test jar").unwrap();

    let result = add_mod_rule_from_root(
        &root,
        &AddModRuleInput {
            modlist_name: "Pack".into(),
            mod_id: "../evil".into(),
            source: "local".into(),
            file_name: Some(source_path.to_string_lossy().into_owned()),
        },
    );
    let escaped_path = root.join("mod-lists").join("Pack").join("evil.jar");
    let escaped_path_exists = escaped_path.exists();

    fs::remove_dir_all(&root).unwrap();
    assert!(result.is_err());
    assert!(!escaped_path_exists);
}

#[test]
fn add_mod_rule_rejects_duplicate() {
    let root = unique_test_root();
    setup_modlist(&root, "Pack", vec![simple_rule("sodium")]);

    let result = add_mod_rule_from_root(
        &root,
        &AddModRuleInput {
            modlist_name: "Pack".into(),
            mod_id: "sodium".into(),
            source: "modrinth".into(),
            file_name: None,
        },
    );
    assert!(result.is_err());

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn delete_rules_removes_from_tree() {
    let root = unique_test_root();
    setup_modlist(
        &root,
        "Pack",
        vec![
            Rule {
                mod_id: "sodium".into(),
                source: ModSource::Modrinth,
                enabled: true,
                exclude_if: vec![],
                requires: vec![],
                version_rules: vec![],
                custom_configs: vec![],
                alternatives: vec![simple_rule("rubidium")],
            },
            simple_rule("lithium"),
        ],
    );

    delete_rules_from_root(
        &root,
        &DeleteRulesInput {
            modlist_name: "Pack".into(),
            mod_ids: vec!["rubidium".into(), "lithium".into()],
        },
    )
    .unwrap();

    let snapshot = load_editor_snapshot_from_root(&root, "Pack").unwrap();
    assert_eq!(snapshot.rows.len(), 1);
    assert_eq!(snapshot.rows[0].mod_id, "sodium");
    assert!(snapshot.rows[0].alternatives.is_empty());

    fs::remove_dir_all(&root).unwrap();
}

/// Audit 101, editor-logica-5: con «X requires Y» si cancella Y, e il link
/// verso un mod che non c'è più restava nel file — X rossa per sempre, senza
/// nessun link visibile. Vale anche per `exclude_if`, per le alternative che
/// spariscono insieme al mod cancellato, e in tutto l'albero.
#[test]
fn delete_rules_removes_requires_and_exclusions_towards_the_deleted_mods() {
    let root = unique_test_root();
    let mut x = simple_rule("x");
    x.requires = vec!["y".into(), "keep".into()];
    x.exclude_if = vec!["y-alt".into(), "keep".into()];
    let mut nested = simple_rule("nested");
    nested.requires = vec!["y-alt".into()];
    x.alternatives = vec![nested];
    let mut y = simple_rule("y");
    y.alternatives = vec![simple_rule("y-alt")];
    let mut z = simple_rule("z");
    z.requires = vec!["keep".into()];
    setup_modlist(&root, "Pack", vec![x, y, simple_rule("keep"), z]);

    delete_rules_from_root(
        &root,
        &DeleteRulesInput {
            modlist_name: "Pack".into(),
            mod_ids: vec!["y".into()],
        },
    )
    .unwrap();

    let modlist = load_modlist(&root, "Pack").unwrap();
    let x = modlist.find_rule("x").unwrap();
    assert_eq!(x.requires, vec!["keep".to_string()]);
    assert_eq!(x.exclude_if, vec!["keep".to_string()]);
    assert!(modlist.find_rule("nested").unwrap().requires.is_empty());
    assert!(modlist.find_rule("y-alt").is_none());
    // Un link verso un mod che resta non si tocca.
    assert_eq!(
        modlist.find_rule("z").unwrap().requires,
        vec!["keep".to_string()]
    );

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn reorder_rules_changes_order() {
    let root = unique_test_root();
    setup_modlist(
        &root,
        "Pack",
        vec![simple_rule("a"), simple_rule("b"), simple_rule("c")],
    );

    reorder_rules_from_root(
        &root,
        &ReorderRulesInput {
            modlist_name: "Pack".into(),
            ordered_mod_ids: vec!["c".into(), "a".into(), "b".into()],
        },
    )
    .unwrap();

    let snapshot = load_editor_snapshot_from_root(&root, "Pack").unwrap();
    let ids: Vec<&str> = snapshot.rows.iter().map(|r| r.mod_id.as_str()).collect();
    assert_eq!(ids, vec!["c", "a", "b"]);

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn add_and_remove_alternative() {
    let root = unique_test_root();
    setup_modlist(&root, "Pack", vec![simple_rule("sodium")]);

    add_alternative_from_root(
        &root,
        &AddAlternativeInput {
            modlist_name: "Pack".into(),
            parent_mod_id: "sodium".into(),
            mod_id: "rubidium".into(),
            source: "modrinth".into(),
        },
    )
    .unwrap();

    let snapshot = load_editor_snapshot_from_root(&root, "Pack").unwrap();
    assert_eq!(snapshot.rows[0].alternatives.len(), 1);

    remove_alternative_from_root(
        &root,
        &RemoveAlternativeInput {
            modlist_name: "Pack".into(),
            parent_mod_id: "sodium".into(),
            alt_mod_id: "rubidium".into(),
        },
    )
    .unwrap();

    let snapshot = load_editor_snapshot_from_root(&root, "Pack").unwrap();
    assert!(snapshot.rows[0].alternatives.is_empty());

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn save_incompatibilities_rewrites_exclude_if() {
    let root = unique_test_root();
    setup_modlist(
        &root,
        "Pack",
        vec![simple_rule("sodium"), simple_rule("embeddium")],
    );

    save_incompatibilities_from_root(
        &root,
        &SaveIncompatibilitiesInput {
            modlist_name: "Pack".into(),
            rules: vec![IncompatibilityRuleInput {
                winner_id: "sodium".into(),
                loser_id: "embeddium".into(),
            }],
        },
    )
    .unwrap();

    let snapshot = load_editor_snapshot_from_root(&root, "Pack").unwrap();
    assert_eq!(snapshot.rows[1].exclude_if, vec!["sodium"]);
    assert_eq!(snapshot.incompatibilities.len(), 1);

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn save_rule_advanced_updates_fields() {
    let root = unique_test_root();
    setup_modlist(&root, "Pack", vec![simple_rule("sodium")]);

    save_rule_advanced_from_root(
        &root,
        &SaveRuleAdvancedInput {
            modlist_name: "Pack".into(),
            mod_id: "sodium".into(),
            exclude_if: vec!["optifine".into()],
            requires: vec!["fabric-api".into()],
            version_rules: vec![SaveVersionRuleInput {
                kind: "exclude".into(),
                mc_versions: vec!["1.18.2".into()],
                loader: "forge".into(),
            }],
            custom_configs: vec![SaveCustomConfigInput {
                id: "cc-sodium".into(),
                mc_versions: vec!["1.21.1".into()],
                loader: "fabric".into(),
                target_path: "config/sodium.json".into(),
                files: vec!["sodium.json".into()],
            }],
        },
    )
    .unwrap();

    let snapshot = load_editor_snapshot_from_root(&root, "Pack").unwrap();
    assert_eq!(snapshot.rows[0].exclude_if, vec!["optifine"]);
    assert_eq!(snapshot.rows[0].requires, vec!["fabric-api"]);
    assert_eq!(snapshot.rows[0].version_rules.len(), 1);
    assert_eq!(snapshot.rows[0].custom_configs.len(), 1);

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn advanced_batch_with_unknown_id_preserves_config() {
    let root = unique_test_root();
    let mut rule = simple_rule("sodium");
    rule.requires = vec!["fabric-api".into()];
    rule.version_rules = vec![VersionRule {
        kind: VersionRuleKind::Only,
        mc_versions: vec!["1.21.1".into()],
        loader: "fabric".into(),
    }];
    rule.custom_configs = vec![CustomConfig {
        id: "cc-sodium".into(),
        mc_versions: vec!["1.21.1".into()],
        loader: "fabric".into(),
        target_path: "config/sodium.json".into(),
        files: vec!["sodium.json".into()],
    }];
    setup_modlist(&root, "Pack", vec![rule.clone()]);

    let result = save_advanced_batch_from_root(
        &root,
        &SaveAdvancedBatchInput {
            modlist_name: "Pack".into(),
            requires_entries: vec![RequiresEntry {
                mod_id: "ghost".into(),
                requires: vec!["fabric-api".into()],
            }],
            version_rules_entries: vec![],
            custom_configs_entries: vec![],
        },
    );

    assert!(result.is_err());
    let reloaded = load_modlist(&root, "Pack").unwrap();
    let reloaded_rule = reloaded.find_rule("sodium").unwrap();
    assert_eq!(reloaded_rule.requires, rule.requires);
    assert_eq!(reloaded_rule.version_rules, rule.version_rules);
    assert_eq!(reloaded_rule.custom_configs, rule.custom_configs);

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn save_rule_advanced_persists_exclude_if() {
    let root = unique_test_root();
    setup_modlist(&root, "Pack", vec![simple_rule("sodium")]);

    save_rule_advanced_from_root(
        &root,
        &SaveRuleAdvancedInput {
            modlist_name: "Pack".into(),
            mod_id: "sodium".into(),
            exclude_if: vec!["optifine".into()],
            requires: vec![],
            version_rules: vec![],
            custom_configs: vec![],
        },
    )
    .unwrap();

    let reloaded = load_modlist(&root, "Pack").unwrap();
    assert_eq!(
        reloaded.find_rule("sodium").unwrap().exclude_if,
        vec!["optifine"]
    );

    fs::remove_dir_all(&root).unwrap();
}

/// D130: un `rules.json` scritto prima che il custom config avesse un `id` ha
/// questa forma esatta. Deve caricarsi, e chi lo carica gli dà un id e lo salva:
/// senza un'identità stabile i file copiati non hanno una cartella.
#[test]
fn legacy_custom_config_without_id_loads_and_gets_a_persisted_id() {
    let root = unique_test_root();
    let modlist_dir = root.join("mod-lists").join("Pack");
    fs::create_dir_all(&modlist_dir).unwrap();
    fs::write(
        modlist_dir.join("rules.json"),
        r#"{
  "schema_version": 4,
  "modlist_name": "Pack",
  "author": "Author",
  "description": "",
  "rules": [
    {
      "mod_id": "sodium",
      "source": "modrinth",
      "custom_configs": [
        {
          "mc_versions": ["1.21.1"],
          "loader": "fabric",
          "target_path": "config",
          "files": ["sodium-options.json"]
        }
      ]
    }
  ]
}
"#,
    )
    .unwrap();

    let snapshot = load_editor_snapshot_from_root(&root, "Pack").unwrap();
    let assigned = snapshot.rows[0].custom_configs[0].id.clone();
    assert!(
        !assigned.is_empty(),
        "a legacy custom config must get an id"
    );
    validate_path_component(&assigned).expect("the id names a directory");

    let on_disk = load_modlist(&root, "Pack").unwrap();
    assert_eq!(
        on_disk.find_rule("sodium").unwrap().custom_configs[0].id,
        assigned,
        "the assigned id must be saved, not regenerated at every load"
    );
    let reloaded = load_editor_snapshot_from_root(&root, "Pack").unwrap();
    assert_eq!(reloaded.rows[0].custom_configs[0].id, assigned);

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn custom_config_keeps_its_id_through_save_and_reload() {
    let root = unique_test_root();
    setup_modlist(&root, "Pack", vec![simple_rule("sodium")]);

    save_advanced_batch_from_root(
        &root,
        &SaveAdvancedBatchInput {
            modlist_name: "Pack".into(),
            requires_entries: vec![],
            version_rules_entries: vec![],
            custom_configs_entries: vec![CustomConfigsEntry {
                mod_id: "sodium".into(),
                custom_configs: vec![SaveCustomConfigInput {
                    id: "cc-1727890000000".into(),
                    mc_versions: vec![],
                    loader: "any".into(),
                    target_path: String::new(),
                    files: vec!["sodium-options.json".into()],
                }],
            }],
        },
    )
    .unwrap();

    let snapshot = load_editor_snapshot_from_root(&root, "Pack").unwrap();
    assert_eq!(snapshot.rows[0].custom_configs[0].id, "cc-1727890000000");
    let on_disk = load_modlist(&root, "Pack").unwrap();
    assert_eq!(
        on_disk.find_rule("sodium").unwrap().custom_configs[0].id,
        "cc-1727890000000"
    );

    fs::remove_dir_all(&root).unwrap();
}

fn import_input(sources: &[&Path], current_files: &[&str]) -> ImportCustomConfigFilesInput {
    ImportCustomConfigFilesInput {
        modlist_name: "Pack".into(),
        mod_id: "sodium".into(),
        config_id: "cc-1".into(),
        source_paths: sources
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        current_files: current_files.iter().map(|name| name.to_string()).collect(),
    }
}

/// D112: il file scelto viene copiato dentro la mod list; l'originale resta
/// dov'era, con i suoi byte.
#[test]
fn import_custom_config_files_copies_into_the_modlist_and_leaves_the_source() {
    let root = unique_test_root();
    setup_modlist(&root, "Pack", vec![simple_rule("sodium")]);
    let outside = root.join("user-disk");
    fs::create_dir_all(&outside).unwrap();
    let source = outside.join("sodium-options.json");
    fs::write(&source, b"{\"quality\":\"fancy\"}").unwrap();

    let names = import_custom_config_files_from_root(&root, &import_input(&[&source], &[]))
        .expect("import should succeed");

    assert_eq!(names, vec!["sodium-options.json".to_string()]);
    let copied = root
        .join("mod-lists")
        .join("Pack")
        .join("config-files")
        .join("sodium")
        .join("cc-1")
        .join("sodium-options.json");
    assert_eq!(fs::read(&copied).unwrap(), b"{\"quality\":\"fancy\"}");
    assert_eq!(
        fs::read(&source).expect("the source must still be there"),
        b"{\"quality\":\"fancy\"}"
    );

    fs::remove_dir_all(&root).unwrap();
}

/// Due file diversi con lo stesso nome nella stessa scelta: nessuno dei due
/// viene copiato, e l'errore nomina il file.
#[test]
fn import_custom_config_files_rejects_two_different_files_with_one_name() {
    let root = unique_test_root();
    setup_modlist(&root, "Pack", vec![simple_rule("sodium")]);
    let first = root.join("a").join("options.json");
    let second = root.join("b").join("options.json");
    fs::create_dir_all(first.parent().unwrap()).unwrap();
    fs::create_dir_all(second.parent().unwrap()).unwrap();
    fs::write(&first, b"first").unwrap();
    fs::write(&second, b"second").unwrap();

    let error = import_custom_config_files_from_root(&root, &import_input(&[&first, &second], &[]))
        .expect_err("two different files with one name must be rejected");

    assert!(error.to_string().contains("options.json"), "{error:#}");
    assert!(!root
        .join("mod-lists/Pack/config-files/sodium/cc-1/options.json")
        .exists());

    fs::remove_dir_all(&root).unwrap();
}

/// Un nome già nel custom config con byte diversi: il file del launcher non
/// viene sostituito in silenzio. Gli stessi byte invece non sono un conflitto.
#[test]
fn import_custom_config_files_does_not_replace_a_listed_file_with_different_bytes() {
    let root = unique_test_root();
    setup_modlist(&root, "Pack", vec![simple_rule("sodium")]);
    let original = root.join("a").join("options.json");
    let other = root.join("b").join("options.json");
    fs::create_dir_all(original.parent().unwrap()).unwrap();
    fs::create_dir_all(other.parent().unwrap()).unwrap();
    fs::write(&original, b"original").unwrap();
    fs::write(&other, b"other").unwrap();
    import_custom_config_files_from_root(&root, &import_input(&[&original], &[])).unwrap();

    let error =
        import_custom_config_files_from_root(&root, &import_input(&[&other], &["options.json"]))
            .expect_err("a different file under a listed name must be rejected");
    assert!(error.to_string().contains("options.json"), "{error:#}");
    let stored = root.join("mod-lists/Pack/config-files/sodium/cc-1/options.json");
    assert_eq!(fs::read(&stored).unwrap(), b"original");

    let again =
        import_custom_config_files_from_root(&root, &import_input(&[&original], &["options.json"]))
            .expect("the same bytes again are not a conflict");
    assert_eq!(again, vec!["options.json".to_string()]);

    fs::remove_dir_all(&root).unwrap();
}
