use std::collections::HashSet;


#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyRequest {
    pub parent_mod_id: String,
    pub selector: DependencySelector,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencySelector {
    ProjectId { project_id: String },
    VersionId { version_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDependency {
    pub dependency_id: String,
    pub version_id: String,
    pub jar_filename: String,
    pub download_url: String,
    pub file_hash: Option<String>,
    pub date_published: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyLink {
    pub parent_mod_id: String,
    pub dependency_id: String,
    pub specific_version: Option<String>,
    pub jar_filename: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DependencyResolution {
    pub resolved_dependencies: Vec<ResolvedDependency>,
    pub links: Vec<DependencyLink>,
    /// Parent mod project IDs that were excluded because a required dependency
    /// had no compatible version available for the target.
    pub excluded_parents: HashSet<String>,
}
