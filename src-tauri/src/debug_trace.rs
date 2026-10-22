use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tauri::State;

use crate::launcher_paths::LauncherPaths;

const DEBUG_TRACE_FILENAME: &str = "debug-trace.txt";

/// Past this size the trace starts over: it is a debugging aid for the
/// current session, not an archive, and it used to grow forever (shell-10).
const DEBUG_TRACE_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// The trace is a development tool: a release build writes nothing, so a
/// user's data folder never collects it (shell-10).
const DEBUG_TRACE_ENABLED: bool = cfg!(debug_assertions);

#[tauri::command]
pub fn append_debug_trace_command(
    launcher_paths: State<'_, LauncherPaths>,
    entry: String,
) -> Result<(), String> {
    if !DEBUG_TRACE_ENABLED {
        return Ok(());
    }
    append_debug_trace_to_root(launcher_paths.root_dir(), &entry).map_err(|error| error.to_string())
}

pub fn debug_trace_path(root_dir: &Path) -> PathBuf {
    root_dir.join(DEBUG_TRACE_FILENAME)
}

/// Called once at startup. A debug build starts every session from an empty
/// trace; a release build removes the file an older version may have left.
pub fn reset_debug_trace_at_startup(root_dir: &Path) {
    let result = if DEBUG_TRACE_ENABLED {
        clear_debug_trace_at_root(root_dir).map(|_| ())
    } else {
        remove_debug_trace_at_root(root_dir)
    };
    if let Err(error) = result {
        eprintln!("[debug-trace] {error:#}");
    }
}

pub fn append_debug_trace_to_root(root_dir: &Path, entry: &str) -> Result<()> {
    fs::create_dir_all(root_dir)
        .with_context(|| format!("failed to create {}", root_dir.display()))?;

    let trace_path = debug_trace_path(root_dir);
    let too_big = fs::metadata(&trace_path)
        .map(|metadata| metadata.len() >= DEBUG_TRACE_MAX_BYTES)
        .unwrap_or(false);
    if too_big {
        clear_debug_trace_at_root(root_dir)?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&trace_path)
        .with_context(|| format!("failed to open {}", trace_path.display()))?;

    writeln!(file, "{} {}", trace_timestamp(), entry)
        .with_context(|| format!("failed to append {}", trace_path.display()))?;

    Ok(())
}

pub fn clear_debug_trace_at_root(root_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(root_dir)
        .with_context(|| format!("failed to create {}", root_dir.display()))?;

    let trace_path = debug_trace_path(root_dir);
    fs::write(
        &trace_path,
        format!("{} [debug] trace cleared\n", trace_timestamp()),
    )
    .with_context(|| format!("failed to reset {}", trace_path.display()))?;

    Ok(trace_path)
}

fn remove_debug_trace_at_root(root_dir: &Path) -> Result<()> {
    let trace_path = debug_trace_path(root_dir);
    match fs::remove_file(&trace_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to remove {}", trace_path.display()))
        }
    }
}

fn trace_timestamp() -> String {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("[{}.{:03}]", duration.as_secs(), duration.subsec_millis())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        append_debug_trace_to_root, debug_trace_path, remove_debug_trace_at_root,
        DEBUG_TRACE_MAX_BYTES,
    };

    fn temp_root(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "cubic-debug-trace-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn a_trace_past_the_cap_starts_over_instead_of_growing() {
        let root = temp_root("cap");
        fs::create_dir_all(&root).unwrap();
        let path = debug_trace_path(&root);
        fs::write(&path, vec![b'x'; DEBUG_TRACE_MAX_BYTES as usize]).unwrap();

        append_debug_trace_to_root(&root, "[test] after the cap").unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("trace cleared"), "{content}");
        assert!(content.contains("[test] after the cap"), "{content}");
        assert!((content.len() as u64) < DEBUG_TRACE_MAX_BYTES);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn removing_a_trace_that_is_not_there_is_not_an_error() {
        let root = temp_root("remove");
        fs::create_dir_all(&root).unwrap();
        remove_debug_trace_at_root(&root).unwrap();

        fs::write(debug_trace_path(&root), "old session\n").unwrap();
        remove_debug_trace_at_root(&root).unwrap();
        assert!(!debug_trace_path(&root).exists());
        let _ = fs::remove_dir_all(&root);
    }
}
