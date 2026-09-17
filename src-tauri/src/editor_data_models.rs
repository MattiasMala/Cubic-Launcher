use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorSnapshot {
    pub modlist_name: String,
    pub rows: Vec<EditorRow>,
    pub incompatibilities: Vec<IncompatibilityEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorRow {
    pub mod_id: String,
    pub name: String,
    pub source: String,
    pub enabled: bool,
    pub exclude_if: Vec<String>,
    pub requires: Vec<String>,
    pub version_rules: Vec<EditorVersionRule>,
    pub custom_configs: Vec<EditorCustomConfig>,
    /// `data:image/png;base64,…` for a local mod whose jar carries an icon,
    /// `None` for every other row: Modrinth rows get their icon from the
    /// Modrinth CDN in the frontend. Filled by [`crate::mod_icons`].
    pub icon_image: Option<String>,
    pub alternatives: Vec<EditorRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorVersionRule {
    pub kind: String,
    pub mc_versions: Vec<String>,
    pub loader: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorCustomConfig {
    pub mc_versions: Vec<String>,
    pub loader: String,
    pub target_path: String,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IncompatibilityEntry {
    pub winner_id: String,
    pub loser_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddModRuleInput {
    pub modlist_name: String,
    pub mod_id: String,
    pub source: String,
    pub file_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteRulesInput {
    pub modlist_name: String,
    pub mod_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameRuleInput {
    pub modlist_name: String,
    pub mod_id: String,
    pub new_mod_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReorderRulesInput {
    pub modlist_name: String,
    pub ordered_mod_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveAlternativeOrderInput {
    pub modlist_name: String,
    pub parent_mod_id: String,
    pub ordered_alt_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddAlternativeInput {
    pub modlist_name: String,
    pub parent_mod_id: String,
    pub mod_id: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddNestedAlternativeInput {
    pub modlist_name: String,
    pub parent_mod_id: String,
    pub mod_id: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveAlternativeInput {
    pub modlist_name: String,
    pub parent_mod_id: String,
    pub alt_mod_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveIncompatibilitiesInput {
    pub modlist_name: String,
    pub rules: Vec<IncompatibilityRuleInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IncompatibilityRuleInput {
    pub winner_id: String,
    pub loser_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveRuleAdvancedInput {
    pub modlist_name: String,
    pub mod_id: String,
    pub exclude_if: Vec<String>,
    pub requires: Vec<String>,
    pub version_rules: Vec<SaveVersionRuleInput>,
    pub custom_configs: Vec<SaveCustomConfigInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveAdvancedBatchInput {
    pub modlist_name: String,
    pub requires_entries: Vec<RequiresEntry>,
    pub version_rules_entries: Vec<VersionRulesEntry>,
    pub custom_configs_entries: Vec<CustomConfigsEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequiresEntry {
    pub mod_id: String,
    pub requires: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionRulesEntry {
    pub mod_id: String,
    pub version_rules: Vec<SaveVersionRuleInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomConfigsEntry {
    pub mod_id: String,
    pub custom_configs: Vec<SaveCustomConfigInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveVersionRuleInput {
    pub kind: String,
    pub mc_versions: Vec<String>,
    pub loader: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveCustomConfigInput {
    pub mc_versions: Vec<String>,
    pub loader: String,
    pub target_path: String,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToggleRuleEnabledInput {
    pub modlist_name: String,
    pub mod_id: String,
    pub enabled: bool,
}
