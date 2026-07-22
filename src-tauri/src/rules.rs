use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const RULES_FILENAME: &str = "rules.json";

// ── Public types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModSource {
    Modrinth,
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionRuleKind {
    Exclude,
    Only,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionRule {
    pub kind: VersionRuleKind,
    pub mc_versions: Vec<String>,
    pub loader: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomConfig {
    pub mc_versions: Vec<String>,
    pub loader: String,
    pub target_path: String,
    pub files: Vec<String>,
}

fn default_true() -> bool {
    true
}
fn is_true(v: &bool) -> bool {
    *v
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub mod_id: String,
    pub source: ModSource,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_if: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub version_rules: Vec<VersionRule>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_configs: Vec<CustomConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternatives: Vec<Rule>,
}

// ── ModList (in-memory) ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModList {
    pub modlist_name: String,
    pub author: String,
    pub description: String,
    pub rules: Vec<Rule>,
}

// ── v4 file format ───────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct ModListFileV4 {
    schema_version: u32,
    modlist_name: String,
    author: String,
    description: String,
    #[serde(default)]
    rules: Vec<Rule>,
}

impl ModList {
    pub fn read_from_file(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read rules file at {}", path.display()))?;

        let file: ModListFileV4 = serde_json::from_str(&contents).with_context(|| {
            format!(
                "failed to parse rules file at {} — only schema version 4 is supported",
                path.display()
            )
        })?;

        if file.schema_version != 4 {
            bail!(
                "unsupported schema version {} in {} — only version 4 is supported",
                file.schema_version,
                path.display()
            );
        }

        let modlist = Self {
            modlist_name: file.modlist_name,
            author: file.author,
            description: file.description,
            rules: file.rules,
        };

        modlist
            .validate()
            .with_context(|| format!("validation failed for rules file at {}", path.display()))?;

        Ok(modlist)
    }

    pub fn write_to_file(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }

        let file = ModListFileV4 {
            schema_version: 4,
            modlist_name: self.modlist_name.clone(),
            author: self.author.clone(),
            description: self.description.clone(),
            rules: self.rules.clone(),
        };

        let json =
            serde_json::to_string_pretty(&file).context("failed to serialize modlist to JSON")?;

        fs::write(path, format!("{json}\n"))
            .with_context(|| format!("failed to write rules file at {}", path.display()))?;

        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if self.modlist_name.trim().is_empty() {
            bail!("modlist_name cannot be empty");
        }
        if self.author.trim().is_empty() {
            bail!("author cannot be empty");
        }

        let mut seen_ids = HashSet::new();
        for rule in &self.rules {
            validate_rule(rule, &mut seen_ids)?;
        }

        Ok(())
    }

    /// Find a rule anywhere in the tree by mod_id (returns mutable reference).
    pub fn find_rule_mut(&mut self, mod_id: &str) -> Option<&mut Rule> {
        for rule in &mut self.rules {
            if let Some(found) = find_rule_in_tree_mut(rule, mod_id) {
                return Some(found);
            }
        }
        None
    }

    /// Find a rule anywhere in the tree by mod_id (returns immutable reference).
    pub fn find_rule(&self, mod_id: &str) -> Option<&Rule> {
        for rule in &self.rules {
            if let Some(found) = find_rule_in_tree(rule, mod_id) {
                return Some(found);
            }
        }
        None
    }

    /// Check if a mod_id exists anywhere in the tree.
    pub fn contains_mod_id(&self, mod_id: &str) -> bool {
        self.find_rule(mod_id).is_some()
    }
}

fn validate_rule(rule: &Rule, seen_ids: &mut HashSet<String>) -> Result<()> {
    if rule.mod_id.trim().is_empty() {
        bail!("rule mod_id cannot be empty");
    }
    if !seen_ids.insert(rule.mod_id.clone()) {
        bail!("duplicate mod_id '{}' in modlist", rule.mod_id);
    }
    for alt in &rule.alternatives {
        validate_rule(alt, seen_ids)?;
    }
    Ok(())
}

fn find_rule_in_tree_mut<'a>(rule: &'a mut Rule, mod_id: &str) -> Option<&'a mut Rule> {
    if rule.mod_id == mod_id {
        return Some(rule);
    }
    for alt in &mut rule.alternatives {
        if let Some(found) = find_rule_in_tree_mut(alt, mod_id) {
            return Some(found);
        }
    }
    None
}

fn find_rule_in_tree<'a>(rule: &'a Rule, mod_id: &str) -> Option<&'a Rule> {
    if rule.mod_id == mod_id {
        return Some(rule);
    }
    for alt in &rule.alternatives {
        if let Some(found) = find_rule_in_tree(alt, mod_id) {
            return Some(found);
        }
    }
    None
}

// ── Presentation type (standalone, used by modlist_assets) ───────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModlistPresentation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub icon_label: String,
    pub icon_accent: String,
    pub notes: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_image: Option<String>,
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path() -> std::path::PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!("cubic-rules-test-{ts}"))
    }

    fn sample_modlist() -> ModList {
        ModList {
            modlist_name: "Test Pack".into(),
            author: "Author".into(),
            description: "Desc".into(),
            rules: vec![
                Rule {
                    mod_id: "sodium".into(),
                    source: ModSource::Modrinth,
                    enabled: true,
                    exclude_if: vec!["optifine".into()],
                    requires: vec!["lithium".into()],
                    version_rules: vec![VersionRule {
                        kind: VersionRuleKind::Exclude,
                        mc_versions: vec!["1.18.2".into()],
                        loader: "forge".into(),
                    }],
                    custom_configs: vec![CustomConfig {
                        mc_versions: vec!["1.21.1".into()],
                        loader: "fabric".into(),
                        target_path: "config/sodium.json".into(),
                        files: vec!["custom_configs/sodium.json".into()],
                    }],
                    alternatives: vec![Rule {
                        mod_id: "rubidium".into(),
                        source: ModSource::Modrinth,
                        enabled: true,
                        exclude_if: vec![],
                        requires: vec![],
                        version_rules: vec![],
                        custom_configs: vec![],
                        alternatives: vec![],
                    }],
                },
                Rule {
                    mod_id: "OptiFine-1.21.1".into(),
                    source: ModSource::Local,
                    enabled: true,
                    exclude_if: vec![],
                    requires: vec![],
                    version_rules: vec![VersionRule {
                        kind: VersionRuleKind::Only,
                        mc_versions: vec!["1.21.1".into()],
                        loader: "forge".into(),
                    }],
                    custom_configs: vec![],
                    alternatives: vec![],
                },
            ],
        }
    }

    #[test]
    fn roundtrip_serialize_deserialize() {
        let dir = temp_path();
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rules.json");

        let original = sample_modlist();
        original.write_to_file(&path).unwrap();
        let loaded = ModList::read_from_file(&path).unwrap();

        assert_eq!(original, loaded);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn validation_rejects_duplicate_mod_id() {
        let modlist = ModList {
            modlist_name: "Pack".into(),
            author: "Author".into(),
            description: "".into(),
            rules: vec![
                Rule {
                    mod_id: "sodium".into(),
                    source: ModSource::Modrinth,
                    enabled: true,
                    exclude_if: vec![],
                    requires: vec![],
                    version_rules: vec![],
                    custom_configs: vec![],
                    alternatives: vec![],
                },
                Rule {
                    mod_id: "sodium".into(),
                    source: ModSource::Modrinth,
                    enabled: true,
                    exclude_if: vec![],
                    requires: vec![],
                    version_rules: vec![],
                    custom_configs: vec![],
                    alternatives: vec![],
                },
            ],
        };
        assert!(modlist.validate().is_err());
    }

    #[test]
    fn validation_rejects_duplicate_mod_id_in_alternatives() {
        let modlist = ModList {
            modlist_name: "Pack".into(),
            author: "Author".into(),
            description: "".into(),
            rules: vec![Rule {
                mod_id: "sodium".into(),
                source: ModSource::Modrinth,
                enabled: true,
                exclude_if: vec![],
                requires: vec![],
                version_rules: vec![],
                custom_configs: vec![],
                alternatives: vec![Rule {
                    mod_id: "sodium".into(),
                    source: ModSource::Modrinth,
                    enabled: true,
                    exclude_if: vec![],
                    requires: vec![],
                    version_rules: vec![],
                    custom_configs: vec![],
                    alternatives: vec![],
                }],
            }],
        };
        assert!(modlist.validate().is_err());
    }

    #[test]
    fn validation_rejects_empty_mod_id() {
        let modlist = ModList {
            modlist_name: "Pack".into(),
            author: "Author".into(),
            description: "".into(),
            rules: vec![Rule {
                mod_id: "".into(),
                source: ModSource::Modrinth,
                enabled: true,
                exclude_if: vec![],
                requires: vec![],
                version_rules: vec![],
                custom_configs: vec![],
                alternatives: vec![],
            }],
        };
        assert!(modlist.validate().is_err());
    }

    #[test]
    fn validation_rejects_empty_modlist_name() {
        let modlist = ModList {
            modlist_name: "".into(),
            author: "Author".into(),
            description: "".into(),
            rules: vec![],
        };
        assert!(modlist.validate().is_err());
    }

    #[test]
    fn validation_rejects_empty_author() {
        let modlist = ModList {
            modlist_name: "Pack".into(),
            author: "  ".into(),
            description: "".into(),
            rules: vec![],
        };
        assert!(modlist.validate().is_err());
    }

    #[test]
    fn read_rejects_wrong_schema_version() {
        let dir = temp_path();
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rules.json");

        fs::write(
            &path,
            r#"{"schema_version": 3, "modlist_name": "P", "author": "A", "description": "", "rules": []}"#,
        )
        .unwrap();

        assert!(ModList::read_from_file(&path).is_err());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn find_rule_locates_nested_alternative() {
        let modlist = sample_modlist();
        assert!(modlist.find_rule("rubidium").is_some());
        assert!(modlist.find_rule("nonexistent").is_none());
    }

    #[test]
    fn contains_mod_id_checks_full_tree() {
        let modlist = sample_modlist();
        assert!(modlist.contains_mod_id("sodium"));
        assert!(modlist.contains_mod_id("rubidium"));
        assert!(modlist.contains_mod_id("OptiFine-1.21.1"));
        assert!(!modlist.contains_mod_id("iris"));
    }
}
