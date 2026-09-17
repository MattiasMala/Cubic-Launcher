//! The icon of a locally imported mod jar.
//!
//! A mod added from a jar on disk has no Modrinth project, so the frontend had
//! nothing to draw and fell back to a package glyph. The icon is almost always
//! inside the jar: `fabric.mod.json`'s `icon` for Fabric, `logoFile` in
//! `META-INF/mods.toml` (or `META-INF/neoforge.mods.toml`) for Forge and
//! NeoForge — the three loaders [`crate::resolver::parse_mod_loader`] accepts.
//!
//! **The extracted PNG is a cache, not data** (decision D50). It is written
//! under the mod list's `.cubic/icons/local-jars/`, the convention
//! [`crate::local_content_packs`] already uses for pack icons, and it is found
//! again by **convention** from the rule's `mod_id` — nothing is recorded in
//! `rules.json`. The reason is portability: `rules.json` carries
//! `modlist_name`, `author` and `description` and is the file users share, and
//! a path to a PNG on this machine means nothing on the machine that receives
//! it.
//!
//! The price of storing nothing is that a jar **without** an icon is reopened
//! on every load, because there is no negative to remember: measured at 3–30 ms
//! per jar (91 of 232 real jars declare no icon), accepted with D50.

use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::local_content_packs::{
    icon_relative_path, is_png, read_icon_data_url, write_icon, MAX_PACK_ICON_BYTES,
};

/// Directory, inside a mod list, holding the jars of locally imported mods.
/// Written by [`crate::editor_data::add_mod_rule_from_root`] as
/// `<mod_id>.jar`.
pub const LOCAL_JARS_DIR: &str = "local-jars";

/// The jar of a local rule: `mod-lists/<name>/local-jars/<mod_id>.jar`.
pub fn local_jar_path(modlist_dir: &Path, mod_id: &str) -> PathBuf {
    modlist_dir
        .join(LOCAL_JARS_DIR)
        .join(format!("{mod_id}.jar"))
}

/// Cache path of the icon, relative to the mod list.
pub fn icon_cache_relative_path(mod_id: &str) -> String {
    icon_relative_path(LOCAL_JARS_DIR, &format!("{mod_id}.jar"))
}

/// The icon of a local rule as a `data:image/png;base64,…`, extracting it from
/// the jar the first time and reading the cached PNG afterwards.
///
/// The cache is invalidated by **mtime**: the jar being newer than the PNG next
/// to it means the jar was replaced. That happens with the same `mod_id`,
/// because deleting a rule leaves `local-jars/<mod_id>.jar` on disk, so
/// re-importing a different jar under the same name overwrites the bytes and
/// would otherwise keep showing the previous mod's icon. One `stat` per local
/// row buys not having to remember anything.
///
/// Every failure is `None`, which the frontend draws as the same placeholder a
/// jar without an icon gets: a jar with no icon is normal, not an error, so
/// nothing is logged and nothing is surfaced.
pub fn local_mod_icon_data_url(modlist_dir: &Path, mod_id: &str) -> Option<String> {
    let relative = icon_cache_relative_path(mod_id);
    let jar = local_jar_path(modlist_dir, mod_id);
    if !jar_is_newer_than_cache(&jar, &modlist_dir.join(&relative)) {
        if let Some(url) = read_icon_data_url(modlist_dir, &relative) {
            return Some(url);
        }
    }

    let bytes = read_icon_from_jar(&jar).ok().flatten()?;
    write_icon(modlist_dir, LOCAL_JARS_DIR, &format!("{mod_id}.jar"), &bytes).ok()?;
    read_icon_data_url(modlist_dir, &relative)
}

/// Whether the cached icon is stale. A missing jar is never "newer": the cache
/// is then the only thing left that can answer.
fn jar_is_newer_than_cache(jar: &Path, cache: &Path) -> bool {
    let (Ok(jar), Ok(cache)) = (
        std::fs::metadata(jar).and_then(|meta| meta.modified()),
        std::fs::metadata(cache).and_then(|meta| meta.modified()),
    ) else {
        return false;
    };
    jar > cache
}

/// The icon bytes declared by a jar, when it has a usable one.
///
/// `Ok(None)` covers every "this jar has no icon we can use": no loader
/// metadata, no icon declared, a path that leaves the archive, a member that is
/// not in the jar, something bigger than [`MAX_PACK_ICON_BYTES`], or bytes that
/// are not a PNG. `Err` is reserved for the jar not being readable at all.
pub fn read_icon_from_jar(jar_path: &Path) -> Result<Option<Vec<u8>>> {
    let file = std::fs::File::open(jar_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    let source = jar_path.display().to_string();

    let declared = match declared_icon_path(&mut archive, &source)? {
        Some(path) => path,
        None => return Ok(None),
    };

    let member = match sanitize_member_path(&declared) {
        Some(member) => member,
        None => return Ok(None),
    };

    Ok(read_png_member(&mut archive, &member))
}

/// The icon path a jar declares, for the three supported loaders.
///
/// Fabric first, because `fabric.mod.json` is the only one of the three that is
/// also a dependency manifest the launch path already reads; then NeoForge's
/// own file, then the Forge one. A multi-loader jar (Sinytra-style, both
/// manifests) answers with the Fabric icon **when the Fabric side declares
/// one** and falls through to the toml otherwise: the point is to find an
/// icon, and one manifest staying silent is not a reason to ignore the other.
fn declared_icon_path<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    source: &str,
) -> Result<Option<String>> {
    if let Some(metadata) =
        crate::launch_preview::fabric::read_embedded_fabric_metadata(archive, source)?
    {
        if let Some(path) = fabric_icon_path(&metadata) {
            return Ok(Some(path));
        }
    }

    for member in ["META-INF/neoforge.mods.toml", "META-INF/mods.toml"] {
        let text = {
            let mut file = match archive.by_name(member) {
                Ok(file) => file,
                Err(_) => continue,
            };
            let mut text = String::new();
            if file.read_to_string(&mut text).is_err() {
                return Ok(None);
            }
            text
        };
        return Ok(logo_file_path(&text));
    }

    Ok(None)
}

/// `fabric.mod.json`'s `icon`: either a path, or a map of square size to path,
/// of which the largest is taken — a 12 px mosaic tile and a 32 px row both
/// look better downscaled than upscaled.
fn fabric_icon_path(metadata: &serde_json::Value) -> Option<String> {
    match metadata.get("icon")? {
        serde_json::Value::String(path) => non_empty(path),
        serde_json::Value::Object(by_size) => by_size
            .iter()
            .filter_map(|(size, path)| {
                Some((size.parse::<u64>().unwrap_or(0), non_empty(path.as_str()?)?))
            })
            .max_by_key(|(size, _)| *size)
            .map(|(_, path)| path),
        _ => None,
    }
}

/// `logoFile` out of a `mods.toml`, read as lines rather than as TOML.
///
/// The first occurrence wins. On the 145 real Forge/NeoForge jars that declare
/// it, 129 put it inside `[[mods]]` and 16 at the top level and **none** does
/// both, so precedence between the two is not observable here and is not
/// invented: see the report's unverified assumptions.
fn logo_file_path(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("logoFile")?.trim_start();
        let rest = rest.strip_prefix('=')?.trim_start();
        let quote = rest.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let rest = &rest[quote.len_utf8()..];
        non_empty(&rest[..rest.find(quote)?])
    })
}

/// An archive-relative member path, or `None` when the declared path is not one.
///
/// A leading `/` is dropped (jars declare both forms), and anything with a `..`
/// component is refused: one real jar in the user's cache declares
/// `../assets/matowos_invisible_armor/icon.png`, which is a path out of the
/// archive and not an icon this jar owns.
fn sanitize_member_path(declared: &str) -> Option<String> {
    let cleaned = declared.trim().trim_start_matches('/');
    if cleaned.is_empty() || cleaned.split('/').any(|part| part == "..") {
        return None;
    }
    Some(cleaned.to_string())
}

/// The member's bytes, when it exists, fits the cap and is really a PNG.
fn read_png_member<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    member: &str,
) -> Option<Vec<u8>> {
    let mut file = archive.by_name(member).ok()?;
    if file.size() > MAX_PACK_ICON_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut bytes).ok()?;
    if !is_png(&bytes) {
        return None;
    }
    Some(bytes)
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;

    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n and then some bytes";

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cubic-mod-icons-{name}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A jar with the given members, written for real so the extractor is
    /// exercised through `zip`, not through a stub.
    fn write_jar(path: &Path, members: &[(&str, &[u8])]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for (name, bytes) in members {
            zip.start_file(*name, zip::write::FileOptions::default())
                .unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }

    fn jar_icon(name: &str, members: &[(&str, &[u8])]) -> Option<Vec<u8>> {
        let dir = temp_dir(name);
        let jar = dir.join("mod.jar");
        write_jar(&jar, members);
        let icon = read_icon_from_jar(&jar).unwrap();
        fs::remove_dir_all(&dir).ok();
        icon
    }

    #[test]
    fn fabric_icon_comes_from_the_declared_member() {
        let icon = jar_icon(
            "fabric",
            &[
                ("fabric.mod.json", br#"{"id":"m","icon":"assets/m/icon.png"}"#),
                ("assets/m/icon.png", PNG),
            ],
        );

        assert_eq!(icon.as_deref(), Some(PNG));
    }

    #[test]
    fn fabric_icon_map_takes_the_largest_size() {
        let big = b"\x89PNG\r\n\x1a\nbig" as &[u8];
        let icon = jar_icon(
            "fabric-map",
            &[
                (
                    "fabric.mod.json",
                    br#"{"id":"m","icon":{"32":"small.png","128":"big.png"}}"#,
                ),
                ("small.png", PNG),
                ("big.png", big),
            ],
        );

        assert_eq!(icon.as_deref(), Some(big));
    }

    #[test]
    fn forge_logo_file_is_read_at_top_level_and_inside_mods() {
        let top = jar_icon(
            "forge-top",
            &[
                (
                    "META-INF/mods.toml",
                    b"modLoader=\"javafml\"\nlogoFile = \"assets/m/icon.png\"\n[[mods]]\nmodId=\"m\"\n",
                ),
                ("assets/m/icon.png", PNG),
            ],
        );
        let per_mod = jar_icon(
            "forge-per-mod",
            &[
                (
                    "META-INF/mods.toml",
                    b"modLoader=\"javafml\"\n[[mods]]\nmodId=\"m\"\nlogoFile = \"icon.png\"\n",
                ),
                ("icon.png", PNG),
            ],
        );

        assert_eq!(top.as_deref(), Some(PNG));
        assert_eq!(per_mod.as_deref(), Some(PNG));
    }

    #[test]
    fn neoforge_manifest_wins_over_the_forge_one() {
        let neo = b"\x89PNG\r\n\x1a\nneo" as &[u8];
        let icon = jar_icon(
            "neoforge",
            &[
                (
                    "META-INF/neoforge.mods.toml",
                    b"[[mods]]\nlogoFile = \"neo.png\"\n",
                ),
                ("META-INF/mods.toml", b"[[mods]]\nlogoFile = \"old.png\"\n"),
                ("neo.png", neo),
                ("old.png", PNG),
            ],
        );

        assert_eq!(icon.as_deref(), Some(neo));
    }

    #[test]
    fn a_multi_loader_jar_falls_through_to_the_toml_when_fabric_declares_no_icon() {
        let icon = jar_icon(
            "multi-loader",
            &[
                ("fabric.mod.json", br#"{"id":"m","version":"1.0"}"#),
                ("META-INF/mods.toml", b"[[mods]]\nlogoFile = \"icon.png\"\n"),
                ("icon.png", PNG),
            ],
        );

        assert_eq!(icon.as_deref(), Some(PNG));
    }

    #[test]
    fn a_jar_without_loader_metadata_has_no_icon() {
        let icon = jar_icon(
            "no-metadata",
            &[("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\n"), ("icon.png", PNG)],
        );

        assert_eq!(icon, None);
    }

    #[test]
    fn a_declared_member_missing_from_the_jar_has_no_icon() {
        let icon = jar_icon(
            "missing-member",
            &[("META-INF/mods.toml", b"[[mods]]\nlogoFile = \"icon.png\"\n")],
        );

        assert_eq!(icon, None);
    }

    #[test]
    fn a_path_leaving_the_archive_is_refused() {
        let icon = jar_icon(
            "traversal",
            &[
                (
                    "META-INF/mods.toml",
                    b"[[mods]]\nlogoFile = \"../assets/m/icon.png\"\n",
                ),
                ("../assets/m/icon.png", PNG),
            ],
        );

        assert_eq!(icon, None);
    }

    #[test]
    fn a_declared_icon_that_is_not_a_png_has_no_icon() {
        let icon = jar_icon(
            "not-png",
            &[
                ("META-INF/mods.toml", b"[[mods]]\nlogoFile = \"logo.gif\"\n"),
                ("logo.gif", b"GIF89a and the rest"),
            ],
        );

        assert_eq!(icon, None);
    }

    #[test]
    fn an_icon_over_the_cap_has_no_icon() {
        let oversized = {
            let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
            bytes.resize(MAX_PACK_ICON_BYTES as usize + 1, 0);
            bytes
        };
        let icon = jar_icon(
            "oversized",
            &[
                ("fabric.mod.json", br#"{"id":"m","icon":"icon.png"}"#),
                ("icon.png", &oversized),
            ],
        );

        assert_eq!(icon, None);
    }

    #[test]
    fn a_leading_slash_still_resolves() {
        let icon = jar_icon(
            "leading-slash",
            &[
                ("fabric.mod.json", br#"{"id":"m","icon":"/icon.png"}"#),
                ("icon.png", PNG),
            ],
        );

        assert_eq!(icon.as_deref(), Some(PNG));
    }

    #[test]
    fn the_second_read_of_an_icon_comes_from_the_cache_and_not_from_the_jar() {
        let modlist = temp_dir("cache");
        let jar = local_jar_path(&modlist, "my-mod");
        write_jar(
            &jar,
            &[
                ("fabric.mod.json", br#"{"id":"m","icon":"icon.png"}"#),
                ("icon.png", PNG),
            ],
        );

        let first = local_mod_icon_data_url(&modlist, "my-mod");
        let cached = modlist.join(icon_cache_relative_path("my-mod"));
        let cached_exists = cached.is_file();
        // Remove the jar: only the cache can answer now.
        fs::remove_file(&jar).unwrap();
        let second = local_mod_icon_data_url(&modlist, "my-mod");
        fs::remove_dir_all(&modlist).ok();

        assert!(cached_exists, "the icon should have been cached on first read");
        assert!(first.as_deref().unwrap().starts_with("data:image/png;base64,"));
        assert_eq!(first, second);
    }

    #[test]
    fn a_jar_replaced_under_the_same_name_gets_its_new_icon() {
        let replacement = b"\x89PNG\r\n\x1a\nreplacement" as &[u8];
        let modlist = temp_dir("replaced");
        let jar = local_jar_path(&modlist, "my-mod");
        write_jar(
            &jar,
            &[
                ("fabric.mod.json", br#"{"id":"m","icon":"icon.png"}"#),
                ("icon.png", PNG),
            ],
        );
        let first = local_mod_icon_data_url(&modlist, "my-mod");

        // Same path, different mod: this is what deleting a rule (which leaves the
        // jar behind) and re-importing another jar under the same mod_id does.
        write_jar(
            &jar,
            &[
                ("fabric.mod.json", br#"{"id":"m","icon":"icon.png"}"#),
                ("icon.png", replacement),
            ],
        );
        let cache = modlist.join(icon_cache_relative_path("my-mod"));
        let older = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        fs::File::open(&cache)
            .unwrap()
            .set_modified(older)
            .unwrap();

        let second = local_mod_icon_data_url(&modlist, "my-mod");
        let cached_bytes = fs::read(&cache).unwrap();
        fs::remove_dir_all(&modlist).ok();

        assert_ne!(first, second, "a replaced jar must not keep the old icon");
        assert_eq!(cached_bytes, replacement);
    }

    #[test]
    fn a_rule_without_a_jar_has_no_icon_and_writes_nothing() {
        let modlist = temp_dir("no-jar");

        let url = local_mod_icon_data_url(&modlist, "ghost");
        let cubic_dir = modlist.join(".cubic");
        let wrote_anything = cubic_dir.exists();
        fs::remove_dir_all(&modlist).ok();

        assert_eq!(url, None);
        assert!(!wrote_anything, "a missing jar must not create a cache tree");
    }
}
