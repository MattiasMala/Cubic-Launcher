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

#[test]
fn rename_rule_updates_all_references() {
    let root = unique_test_root();
    setup_modlist(
        &root,
        "Pack",
        vec![
            simple_rule("sodium"),
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

    rename_rule_from_root(
        &root,
        &RenameRuleInput {
            modlist_name: "Pack".into(),
            mod_id: "sodium".into(),
            new_mod_id: "sodium-extra".into(),
        },
    )
    .unwrap();

    let snapshot = load_editor_snapshot_from_root(&root, "Pack").unwrap();
    assert_eq!(snapshot.rows[0].mod_id, "sodium-extra");
    assert_eq!(snapshot.rows[1].exclude_if, vec!["sodium-extra"]);

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
