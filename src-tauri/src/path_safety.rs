//! Path containment helpers shared across every filesystem sink that consumes
//! externally-influenced names or filenames (user-supplied mod-list/instance
//! names, and filenames coming from remote metadata: Modrinth, Adoptium, Mojang
//! and loader manifests).
//!
//! Two primitives:
//! - [`validate_path_component`] for values that MUST be a single path segment
//!   (a mod-list name, a `mod_id`, a jar filename, an archive filename).
//! - [`contained_join`] for values that legitimately contain separators (a
//!   Maven-style library path, an asset `prefix/hash`, an IPC relative path) but
//!   MUST stay within a trusted base directory.
//!
//! Both are purely lexical (no filesystem access), so they also protect paths
//! that do not exist yet (e.g. a directory about to be created).

use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Result};

/// Validate that `name` is a safe single path component.
///
/// Rejects: empty, `.`, `..`, any `/` or `\` separator, absolute/prefix markers,
/// and embedded NUL bytes. Ordinary names with spaces, unicode or punctuation
/// (e.g. `"Cubic Vanilla+"`, `"Sky Pack"`) are accepted.
pub fn validate_path_component(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("path component cannot be empty");
    }
    if name == "." || name == ".." {
        bail!("path component '{name}' is not allowed");
    }
    if name.contains('/') || name.contains('\\') {
        bail!("path component '{name}' must not contain a path separator");
    }
    if name.contains('\0') {
        bail!("path component '{name}' must not contain a null byte");
    }

    // A safe single component parses to exactly one `Normal` component. This
    // also rejects Windows drive prefixes (e.g. `C:`) and root markers.
    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(component)), None) if component == name => Ok(()),
        _ => bail!("path component '{name}' is not a valid single path segment"),
    }
}

/// Characters a **new** mod-list name may not contain, on every platform.
///
/// `< > : " / \ | ? *` are illegal in a Windows file name, and the name is a
/// folder: a mod list made on Linux would not survive an export to Windows
/// (`windows-5`). `;` and `:` are the classpath separators of Windows and
/// Linux: the loader's libraries live under a path that contains the name,
/// and `-cp` would split it in two (`r2-percorsi-non-ascii-e-limiti-windows-1`).
pub const MODLIST_NAME_FORBIDDEN_CHARS: &[char] =
    &['<', '>', ':', '"', '/', '\\', '|', '?', '*', ';'];

/// The device names Windows reserves, whatever the extension: `CON`,
/// `con.txt` and `Con .zip` are all the console.
const WINDOWS_RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
    "COM8", "COM9", "COM¹", "COM²", "COM³", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6",
    "LPT7", "LPT8", "LPT9", "LPT¹", "LPT²", "LPT³",
];

/// Validate the name of a mod list **about to be made** (create, import).
///
/// Stricter than [`validate_path_component`], and the same on every platform:
/// besides being one path segment, the name must be a legal Windows folder
/// name and must not hold a classpath separator ([`MODLIST_NAME_FORBIDDEN_CHARS`],
/// control characters, a reserved device name, a trailing dot or space).
/// Only new names go through here: a mod list that already exists keeps
/// working under its name, so delete, launch and the editor still use
/// [`validate_path_component`].
pub fn validate_new_modlist_name(name: &str) -> Result<()> {
    let refuse = |reason: String| -> Result<()> {
        bail!("'{name}' cannot be used as a mod list name: {reason}")
    };

    if name.trim().is_empty() {
        return refuse("the name is empty".to_string());
    }
    if let Some(found) = name
        .chars()
        .find(|c| MODLIST_NAME_FORBIDDEN_CHARS.contains(c) || c.is_control())
    {
        let shown = if found.is_control() {
            "a control character".to_string()
        } else {
            format!("'{found}'")
        };
        return refuse(format!(
            "it contains {shown}. A mod list name cannot contain any of < > : \" / \\ | ? * ;"
        ));
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return refuse("it cannot end with a dot or a space".to_string());
    }
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    if WINDOWS_RESERVED_NAMES
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(stem))
    {
        return refuse(format!("'{stem}' is a name Windows reserves for a device"));
    }

    validate_path_component(name).or_else(|error| refuse(error.to_string()))
}

/// Join `relative` onto `base`, guaranteeing the result stays within `base`.
///
/// Rejects absolute paths, drive prefixes, and any `..` traversal that would
/// escape `base`. `.` segments are ignored, and interior `..` that stays within
/// `base` is permitted. Returns the contained absolute-relative-to-base path.
pub fn contained_join(base: &Path, relative: &str) -> Result<PathBuf> {
    let relative_path = Path::new(relative);
    let mut result = base.to_path_buf();
    let mut depth: usize = 0;

    for component in relative_path.components() {
        match component {
            Component::Normal(part) => {
                result.push(part);
                depth += 1;
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if depth == 0 {
                    bail!("path '{relative}' escapes the containment root");
                }
                depth -= 1;
                result.pop();
            }
            Component::Prefix(_) | Component::RootDir => {
                bail!("path '{relative}' must be a relative path");
            }
        }
    }

    Ok(result)
}

/// Normalize a directory the user typed, relative to a root the caller picks
/// (a custom config's «Path», relative to the instance), into the one form
/// that is stored: `/` as the separator, no `.` segments, no trailing slash.
///
/// A backslash is a separator here, on every platform: `config\create` typed
/// on Windows means the same folder on Linux, not a folder whose name has a
/// backslash in it. Refused, with the reason: an absolute path (a leading `/`
/// or `\`, a drive like `C:`), any `..` segment — even one that would stay
/// inside the root, since the stored value is shown back to the user and
/// should say where the files go — a `:` and a NUL byte.
///
/// Empty (or only spaces) stays empty, which the caller gives its own meaning;
/// one made only of `.` segments is the root itself, `.`.
pub fn normalize_relative_dir(path: &str) -> Result<String> {
    let unified = path.trim().replace('\\', "/");
    if unified.is_empty() {
        return Ok(String::new());
    }
    if unified.starts_with('/') {
        bail!("path '{path}' must be relative, not start with a separator");
    }
    if unified.contains('\0') {
        bail!("path '{path}' must not contain a null byte");
    }

    let mut parts = Vec::new();
    for part in unified.split('/') {
        match part {
            "" | "." => {}
            ".." => bail!("path '{path}' must not contain '..'"),
            part if part.contains(':') => {
                bail!("path '{path}' must be relative: '{part}' looks like a drive or is not a valid folder name")
            }
            part => parts.push(part),
        }
    }

    if parts.is_empty() {
        return Ok(".".to_string());
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        contained_join, normalize_relative_dir, validate_new_modlist_name, validate_path_component,
    };

    #[test]
    fn a_new_mod_list_name_must_be_legal_on_windows_and_in_a_classpath() {
        for good in [
            "Cubic Vanilla+",
            "Sky Pack",
            "日本語パック",
            "Mañana",
            "Pack (1)",
            "v1.2 final",
            "CONSOLE",
            "COM10",
            "Nul Pack",
        ] {
            validate_new_modlist_name(good).expect(good);
        }

        for bad in [
            "Create: Above and Beyond",
            "Create; Astral",
            "a<b",
            "a>b",
            "say \"hi\"",
            "a|b",
            "why?",
            "star*",
            "a/b",
            "a\\b",
            "tab\there",
            "CON",
            "con",
            "Con.txt",
            "NUL .zip",
            "COM1",
            "lpt9",
            "COM¹",
            "Pack.",
            "Pack ",
            "..",
            "",
        ] {
            let error = validate_new_modlist_name(bad).expect_err(bad);
            assert!(
                error
                    .to_string()
                    .contains("cannot be used as a mod list name"),
                "{bad}: {error}"
            );
        }
    }

    #[test]
    fn normalize_relative_dir_unifies_separators_and_drops_dots() {
        assert_eq!(normalize_relative_dir("").unwrap(), "");
        assert_eq!(normalize_relative_dir("   ").unwrap(), "");
        assert_eq!(normalize_relative_dir("config").unwrap(), "config");
        assert_eq!(
            normalize_relative_dir("config\\create").unwrap(),
            "config/create"
        );
        assert_eq!(
            normalize_relative_dir(" ./config//create/ ").unwrap(),
            "config/create"
        );
        assert_eq!(normalize_relative_dir("./").unwrap(), ".");
        assert_eq!(
            normalize_relative_dir("shaderpacks/Complementary Reimagined").unwrap(),
            "shaderpacks/Complementary Reimagined"
        );
    }

    #[test]
    fn normalize_relative_dir_refuses_what_leaves_the_root() {
        for bad in [
            "/etc",
            "\\config",
            "C:\\Users\\x\\config",
            "C:config",
            "../outside",
            "config/../../outside",
            "config\\..\\other",
            "with\0null",
        ] {
            normalize_relative_dir(bad).expect_err(bad);
        }
    }

    #[test]
    fn validate_path_component_accepts_ordinary_names() {
        validate_path_component("Cubic Vanilla+").expect("space and plus should be allowed");
        validate_path_component("Sky Pack").expect("space should be allowed");
        validate_path_component("sodium").expect("simple name should be allowed");
        validate_path_component("mod-id_1.2.3.jar").expect("filename should be allowed");
        validate_path_component("日本語パック").expect("unicode should be allowed");
    }

    #[test]
    fn validate_path_component_rejects_traversal_and_separators() {
        validate_path_component("").expect_err("empty must be rejected");
        validate_path_component(".").expect_err("dot must be rejected");
        validate_path_component("..").expect_err("dotdot must be rejected");
        validate_path_component("../evil").expect_err("parent traversal must be rejected");
        validate_path_component("a/b").expect_err("forward slash must be rejected");
        validate_path_component("a\\b").expect_err("backslash must be rejected");
        validate_path_component("/etc/passwd").expect_err("absolute must be rejected");
        validate_path_component("with\0null").expect_err("null byte must be rejected");
    }

    #[test]
    fn contained_join_keeps_relative_paths_within_base() {
        let base = Path::new("/launcher/libraries");
        let joined = contained_join(base, "net/minecraftforge/forge/forge.jar")
            .expect("legitimate maven path should be contained");
        assert_eq!(
            joined,
            Path::new("/launcher/libraries/net/minecraftforge/forge/forge.jar")
        );

        let with_curdir =
            contained_join(base, "./ab/cd").expect("current-dir segments should be ignored");
        assert_eq!(with_curdir, Path::new("/launcher/libraries/ab/cd"));

        let interior_parent = contained_join(base, "a/b/../c")
            .expect("interior parent staying within base should be allowed");
        assert_eq!(interior_parent, Path::new("/launcher/libraries/a/c"));
    }

    #[test]
    fn contained_join_rejects_escapes_and_absolutes() {
        let base = Path::new("/launcher/libraries");
        contained_join(base, "../../etc/passwd").expect_err("parent escape must be rejected");
        contained_join(base, "a/../../etc").expect_err("net escape must be rejected");
        contained_join(base, "/etc/passwd").expect_err("absolute must be rejected");
    }
}
