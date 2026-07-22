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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{contained_join, validate_path_component};

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

        let with_curdir = contained_join(base, "./ab/cd")
            .expect("current-dir segments should be ignored");
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
