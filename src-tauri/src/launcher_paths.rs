use std::fs;
use std::path::PathBuf;

use anyhow::Result;

use crate::database::DATABASE_FILENAME;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherPaths {
    root_dir: PathBuf,
    cache_dir: PathBuf,
    logs_dir: PathBuf,
    launch_logs_dir: PathBuf,
    mods_cache_dir: PathBuf,
    configs_cache_dir: PathBuf,
    content_packs_cache_dir: PathBuf,
    modlists_dir: PathBuf,
    java_runtimes_dir: PathBuf,
    skins_dir: PathBuf,
    database_path: PathBuf,
}

impl LauncherPaths {
    pub fn new(root_dir: impl Into<PathBuf>) -> Self {
        let root_dir = root_dir.into();
        let cache_dir = root_dir.join("cache");
        let logs_dir = root_dir.join("logs");
        let launch_logs_dir = logs_dir.join("launches");
        let mods_cache_dir = cache_dir.join("mods");
        let configs_cache_dir = cache_dir.join("configs");
        let content_packs_cache_dir = cache_dir.join("content-packs");
        let modlists_dir = root_dir.join("mod-lists");
        let java_runtimes_dir = root_dir.join("java-runtimes");
        let skins_dir = root_dir.join("skins");
        let database_path = root_dir.join(DATABASE_FILENAME);

        Self {
            root_dir,
            cache_dir,
            logs_dir,
            launch_logs_dir,
            mods_cache_dir,
            configs_cache_dir,
            content_packs_cache_dir,
            modlists_dir,
            java_runtimes_dir,
            skins_dir,
            database_path,
        }
    }

    pub fn root_dir(&self) -> &std::path::Path {
        &self.root_dir
    }

    pub fn database_path(&self) -> &std::path::Path {
        &self.database_path
    }

    pub fn modlists_dir(&self) -> &std::path::Path {
        &self.modlists_dir
    }

    pub fn mods_cache_dir(&self) -> &std::path::Path {
        &self.mods_cache_dir
    }

    pub fn mods_cache_loader_dir(&self, mod_loader: &str) -> PathBuf {
        self.mods_cache_dir.join(mod_loader)
    }

    pub fn remote_mod_artifact_path(
        &self,
        mod_loader: &str,
        version_id: &str,
        jar_filename: &str,
    ) -> PathBuf {
        self.mods_cache_loader_dir(mod_loader)
            .join(version_id)
            .join(jar_filename)
    }

    pub fn local_mod_artifact_path(&self, mod_loader: &str, jar_filename: &str) -> PathBuf {
        self.mods_cache_loader_dir(mod_loader)
            .join("local")
            .join(jar_filename)
    }

    pub fn legacy_mod_artifact_path(&self, jar_filename: &str) -> PathBuf {
        self.mods_cache_dir.join(jar_filename)
    }

    pub fn logs_dir(&self) -> &std::path::Path {
        &self.logs_dir
    }

    pub fn launch_logs_dir(&self) -> &std::path::Path {
        &self.launch_logs_dir
    }

    pub fn configs_cache_dir(&self) -> &std::path::Path {
        &self.configs_cache_dir
    }

    pub fn content_packs_cache_dir(&self) -> &std::path::Path {
        &self.content_packs_cache_dir
    }

    pub fn java_runtimes_dir(&self) -> &std::path::Path {
        &self.java_runtimes_dir
    }

    /// `<root>/skins/`: the saved skins, one `<texture_key>.png` each (E7).
    /// Global like `mod-lists/`, because a skin belongs to the player and not
    /// to a mod list; the list of which skins each player saved lives in
    /// `global_settings`, not here.
    pub fn skins_dir(&self) -> &std::path::Path {
        &self.skins_dir
    }

    pub fn mc_cache_dir(&self) -> PathBuf {
        self.cache_dir.join("minecraft")
    }

    pub fn mc_version_dir(&self, version: &str) -> PathBuf {
        self.mc_cache_dir().join(version)
    }

    pub fn mc_libraries_dir(&self) -> PathBuf {
        self.mc_cache_dir().join("libraries")
    }

    pub fn mc_assets_dir(&self) -> PathBuf {
        self.mc_cache_dir().join("assets")
    }

    pub fn create_required_directories(&self) -> Result<()> {
        for directory in [
            &self.root_dir,
            &self.cache_dir,
            &self.logs_dir,
            &self.launch_logs_dir,
            &self.mods_cache_dir,
            &self.configs_cache_dir,
            &self.content_packs_cache_dir,
            &self.modlists_dir,
            &self.java_runtimes_dir,
            &self.skins_dir,
        ] {
            fs::create_dir_all(directory)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::LauncherPaths;

    fn unique_test_root() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();

        env::temp_dir().join(format!("cubic-launcher-paths-test-{timestamp}"))
    }

    #[test]
    fn create_required_directories_builds_expected_structure() {
        let root_dir = unique_test_root();
        let paths = LauncherPaths::new(&root_dir);

        paths
            .create_required_directories()
            .expect("directory initialization should succeed");

        assert!(paths.root_dir.exists(), "root directory should exist");
        assert!(
            paths.mods_cache_dir.exists(),
            "mods cache directory should exist"
        );
        assert!(
            paths.configs_cache_dir.exists(),
            "configs cache directory should exist"
        );
        assert!(
            paths.modlists_dir.exists(),
            "mod-lists directory should exist"
        );
        assert!(
            paths.java_runtimes_dir.exists(),
            "java runtimes directory should exist"
        );

        fs::remove_dir_all(&root_dir).expect("temporary root directory should be removable");
    }

    #[test]
    fn create_required_directories_is_idempotent() {
        let root_dir = unique_test_root();
        let paths = LauncherPaths::new(&root_dir);

        paths
            .create_required_directories()
            .expect("first directory initialization should succeed");
        paths
            .create_required_directories()
            .expect("second directory initialization should also succeed");

        fs::remove_dir_all(&root_dir).expect("temporary root directory should be removable");
    }

    #[test]
    fn database_path_points_to_root_database_file() {
        let root_dir = unique_test_root();
        let paths = LauncherPaths::new(&root_dir);

        assert_eq!(paths.database_path(), root_dir.join("launcher_data.db"));
    }

    /// Saved skins are the player's, not a mod list's: a top-level folder next
    /// to `mod-lists/`, not inside a mod list and not under `.cubic/`.
    #[test]
    fn skins_dir_is_a_top_level_folder_created_with_the_others() {
        let root_dir = unique_test_root();
        let paths = LauncherPaths::new(&root_dir);

        assert_eq!(paths.skins_dir(), root_dir.join("skins"));

        paths
            .create_required_directories()
            .expect("directory initialization should succeed");
        assert!(paths.skins_dir().is_dir(), "the skins folder should exist");

        fs::remove_dir_all(&root_dir).expect("temporary root directory should be removable");
    }
}
