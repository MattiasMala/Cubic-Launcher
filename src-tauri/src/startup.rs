//! What has to hold before the window opens: a data folder the launcher can
//! write, a database it can open, and no other launcher on the same folder.
//!
//! It runs before Tauri on purpose. Inside `setup` an error became a `panic!`,
//! and a Windows release has no console: the window flashed and vanished
//! without a word (r2-primo-avvio-ambiente-degradato-4). Before Tauri the
//! launcher can still show a native dialog of its own and exit cleanly.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::launcher_paths::LauncherPaths;

/// Sits next to the database. Held for the whole life of the process; the
/// system releases it when the process ends, crash or not.
const INSTANCE_LOCK_FILENAME: &str = "launcher.lock";

/// How long a busy lock is retried before saying another launcher has it. The
/// restart after an update starts the new process before the old one is gone.
const INSTANCE_LOCK_PATIENCE: Duration = Duration::from_secs(5);

/// The data folder, ready for the app, and the lock that keeps a second
/// launcher out of it (concorrenza-5).
pub(crate) struct LauncherData {
    pub paths: LauncherPaths,
    pub instance_lock: InstanceLock,
}

/// Kept in Tauri's state so that it lives as long as the app.
pub(crate) struct InstanceLock {
    _file: File,
}

#[derive(Debug)]
pub(crate) enum StartupFailure {
    /// Another launcher holds the same data folder. Two of them would relink
    /// the same `mods/` and rewrite the same shared files under each other's
    /// running game (concorrenza-5). Scoped to the folder, not to the app
    /// identifier: a test run with its own `XDG_DATA_HOME` is not a second
    /// launcher on the user's data.
    AlreadyRunning { root: PathBuf },
    /// The folder or the database cannot be used: what failed, where, and why.
    Unusable {
        summary: &'static str,
        path: PathBuf,
        cause: String,
    },
}

impl StartupFailure {
    fn unusable(summary: &'static str, path: &Path, cause: impl std::fmt::Display) -> Self {
        Self::Unusable {
            summary,
            path: path.to_path_buf(),
            cause: cause.to_string(),
        }
    }

    pub(crate) fn title(&self) -> &'static str {
        match self {
            Self::AlreadyRunning { .. } => "Cubic Launcher is already running",
            Self::Unusable { .. } => "Cubic Launcher could not start",
        }
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::AlreadyRunning { root } => format!(
                "Another Cubic Launcher is already open on this data folder. Switch to its \
                 window instead of opening a second one: two launchers would overwrite each \
                 other's instance files.\n\nData folder: {}",
                root.display()
            ),
            Self::Unusable {
                summary,
                path,
                cause,
            } => format!("{summary}\n\nPath: {}\nCause: {cause}", path.display()),
        }
    }
}

/// `app_local_data_dir()` without a running app: Tauri resolves it as
/// `dirs::data_local_dir()/<identifier>`, and this has to name the same folder.
pub(crate) fn launcher_root(identifier: &str) -> Result<PathBuf, StartupFailure> {
    dirs::data_local_dir()
        .map(|dir| dir.join(identifier))
        .ok_or_else(|| StartupFailure::Unusable {
            summary: "The launcher could not find the user's local data folder.",
            path: PathBuf::from(identifier),
            cause: "the system did not report a local data directory (on Linux: neither \
                    XDG_DATA_HOME nor HOME is usable)"
                .to_string(),
        })
}

pub(crate) fn prepare_launcher_data(root: PathBuf) -> Result<LauncherData, StartupFailure> {
    prepare_launcher_data_within(root, INSTANCE_LOCK_PATIENCE)
}

fn prepare_launcher_data_within(
    root: PathBuf,
    lock_patience: Duration,
) -> Result<LauncherData, StartupFailure> {
    let paths = LauncherPaths::new(root);

    paths.create_required_directories().map_err(|error| {
        StartupFailure::unusable(
            "The launcher could not create its data folder. Check that the folder is writable and the disk is not full.",
            paths.root_dir(),
            format!("{error:#}"),
        )
    })?;

    // Before the database: a second launcher must not migrate it under the
    // first one. Creating the file is also the first real write in the folder.
    let instance_lock = acquire_instance_lock(paths.root_dir(), lock_patience)?;

    crate::debug_trace::reset_debug_trace_at_startup(paths.root_dir());

    crate::database::initialize_database(paths.database_path()).map_err(|error| {
        StartupFailure::unusable(
            "The launcher database could not be opened or updated. It may be damaged, read-only, or the disk may be full.",
            paths.database_path(),
            format!("{error:#}"),
        )
    })?;

    let connection = rusqlite::Connection::open(paths.database_path()).map_err(|error| {
        StartupFailure::unusable(
            "The launcher database could not be opened.",
            paths.database_path(),
            error,
        )
    })?;
    crate::database::migrate_account_tokens_from_profile_data(
        &connection,
        crate::token_storage::KeyringSecretStore::new(),
    )
    .map_err(|error| {
        StartupFailure::unusable(
            "The launcher could not update the saved accounts in its database.",
            paths.database_path(),
            format!("{error:#}"),
        )
    })?;

    Ok(LauncherData {
        paths,
        instance_lock,
    })
}

fn acquire_instance_lock(root: &Path, patience: Duration) -> Result<InstanceLock, StartupFailure> {
    let lock_path = root.join(INSTANCE_LOCK_FILENAME);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|error| {
            StartupFailure::unusable(
                "The launcher could not write in its data folder. Check that the folder is writable and the disk is not full.",
                &lock_path,
                error,
            )
        })?;

    let deadline = Instant::now() + patience;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(InstanceLock { _file: file }),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(TryLockError::WouldBlock) => {
                return Err(StartupFailure::AlreadyRunning {
                    root: root.to_path_buf(),
                })
            }
            Err(TryLockError::Error(error)) => {
                return Err(StartupFailure::unusable(
                    "The launcher could not lock its data folder.",
                    &lock_path,
                    error,
                ))
            }
        }
    }
}

/// Says why on stderr and in a native dialog, then exits. Never returns.
pub(crate) fn report_and_exit(failure: &StartupFailure) -> ! {
    eprintln!("[Startup] {}: {}", failure.title(), failure.message());

    // The automation run (the harness) has no one to click OK, and without a
    // display the GTK dialog could never open, and would wait forever.
    if !crate::launch_preview::automation_mode_enabled() && display_available() {
        let level = match failure {
            StartupFailure::AlreadyRunning { .. } => rfd::MessageLevel::Info,
            StartupFailure::Unusable { .. } => rfd::MessageLevel::Error,
        };
        let _ = rfd::MessageDialog::new()
            .set_level(level)
            .set_title(failure.title())
            .set_description(failure.message())
            .set_buttons(rfd::MessageButtons::Ok)
            .show();
    }

    std::process::exit(1)
}

fn display_available() -> bool {
    if cfg!(target_os = "linux") {
        ["DISPLAY", "WAYLAND_DISPLAY"]
            .iter()
            .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use std::time::Duration;

    use super::{prepare_launcher_data, prepare_launcher_data_within, StartupFailure};

    fn temp_root(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "cubic-startup-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn a_fresh_folder_gets_its_directories_and_database() {
        let root = temp_root("fresh");
        let data = prepare_launcher_data(root.clone()).expect("a fresh folder must start");
        assert!(data.paths.database_path().exists());
        assert!(data.paths.modlists_dir().exists());
        drop(data);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_second_launcher_on_the_same_folder_is_told_one_is_running() {
        let root = temp_root("second");
        let first = prepare_launcher_data(root.clone()).expect("the first launcher starts");

        let second = prepare_launcher_data_within(root.clone(), Duration::from_millis(300));
        match second {
            Err(StartupFailure::AlreadyRunning { root: reported }) => assert_eq!(reported, root),
            Err(other) => panic!("expected AlreadyRunning, got {other:?}"),
            Ok(_) => panic!("a second launcher must not get the folder"),
        }

        // Once the first one is gone the folder is free again.
        drop(first);
        let third = prepare_launcher_data(root.clone()).expect("the lock goes with the process");
        drop(third);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_launcher_restarting_after_an_update_waits_for_the_old_one_to_go() {
        let root = temp_root("restart");
        let old = prepare_launcher_data(root.clone()).expect("the old launcher starts");
        let leaving = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(old);
        });

        let new = prepare_launcher_data_within(root.clone(), Duration::from_secs(5))
            .expect("the lock is free before the patience runs out");
        leaving.join().unwrap();
        drop(new);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_damaged_database_names_its_path_and_the_cause() {
        let root = temp_root("corrupt");
        fs::create_dir_all(&root).unwrap();
        let database = root.join(crate::database::DATABASE_FILENAME);
        fs::write(&database, vec![0x5a_u8; 8192]).unwrap();

        let failure = match prepare_launcher_data(root.clone()) {
            Err(failure) => failure,
            Ok(_) => panic!("a damaged database must not start"),
        };
        let StartupFailure::Unusable { path, cause, .. } = &failure else {
            panic!("expected Unusable, got {failure:?}");
        };
        assert_eq!(path, &database);
        assert!(!cause.is_empty());
        assert!(failure.message().contains(&database.display().to_string()));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_data_folder_that_is_a_file_is_reported_not_panicked() {
        let parent = temp_root("file");
        fs::create_dir_all(&parent).unwrap();
        let root = parent.join("com.cubic.launcher");
        fs::write(&root, b"not a folder").unwrap();

        match prepare_launcher_data(root.clone()) {
            Err(StartupFailure::Unusable { path, .. }) => assert_eq!(path, root),
            Err(other) => panic!("expected Unusable, got {other:?}"),
            Ok(_) => panic!("a file in place of the folder must not start"),
        }
        let _ = fs::remove_dir_all(&parent);
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_data_folder_is_reported_with_its_path() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root("readonly");
        fs::create_dir_all(&root).unwrap();
        // The tree the launcher wants, already there, but nothing writable.
        crate::launcher_paths::LauncherPaths::new(&root)
            .create_required_directories()
            .unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o555)).unwrap();

        let outcome = prepare_launcher_data(root.clone());
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        match outcome {
            Err(StartupFailure::Unusable { path, .. }) => {
                assert_eq!(path, root.join(super::INSTANCE_LOCK_FILENAME))
            }
            Err(other) => panic!("expected Unusable, got {other:?}"),
            Ok(_) => panic!("a read-only folder must not start"),
        }
        let _ = fs::remove_dir_all(&root);
    }
}
