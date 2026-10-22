use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Seek, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::{Deserialize, Serialize};
use tauri::State;
use zip::write::FileOptions;

use crate::atomic_write::write_atomically;
use crate::content_packs::filename_for_type;
use crate::launcher_paths::LauncherPaths;
use crate::local_content_packs::{icon_relative_path, modlist_category_dir};
use crate::mod_icons::LOCAL_JARS_DIR;
use crate::path_safety::{contained_join, validate_path_component};
use crate::rules::{ModlistPresentation, CUSTOM_CONFIG_FILES_DIRNAME, RULES_FILENAME};

const MODLIST_PRESENTATION_FILENAME: &str = "modlist-presentation.json";
const MODLIST_GROUP_LAYOUT_FILENAME: &str = "modlist-editor-groups.json";

/// Tag (formerly "functionalGroup") definition stored in `modlist-editor-groups.json`.
/// Membership is stored here as `mod_ids` — which row IDs this tag applies to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedTag {
    pub id: String,
    pub name: String,
    pub tone: String,
    /// Row IDs of rules/alts that have this tag assigned.
    #[serde(default)]
    pub mod_ids: Vec<String>,
}

/// Aesthetic group (visual section container) stored in `modlist-editor-groups.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedAestheticGroup {
    pub id: String,
    pub name: String,
    pub collapsed: bool,
    #[serde(default)]
    pub block_ids: Vec<String>,
    #[serde(default)]
    pub scope_row_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModlistGroupLayout {
    /// Tag definitions (formerly `functionalGroups`).
    #[serde(default, alias = "functionalGroups")]
    pub tags: Vec<PersistedTag>,
    /// Aesthetic groups (visual section containers).
    #[serde(default)]
    pub aesthetic_groups: Vec<PersistedAestheticGroup>,
    /// Row IDs whose alternatives panel is expanded (persisted across reloads).
    #[serde(default)]
    pub collapsed_alts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveModlistPresentationInput {
    pub modlist_name: String,
    #[serde(default)]
    pub display_name: Option<String>,
    pub icon_label: String,
    pub icon_accent: String,
    pub notes: String,
    #[serde(default)]
    pub icon_image: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportModlistInput {
    pub modlist_name: String,
    pub destination_path: String,
    pub rules_json: bool,
    /// The files of the mod list's custom configs, `config-files/` as it is.
    pub config_files: bool,
    pub resource_packs: bool,
    #[serde(default)]
    pub data_packs: bool,
    #[serde(default)]
    pub shaders: bool,
    /// D150: packs picked one by one, as `<category folder>/<pack>` relative
    /// to the mod list (`resourcepacks/Fresh.zip`). A checked category with
    /// at least one pick exports only those; one with none, its whole folder.
    #[serde(default)]
    pub selected_content_paths: Vec<String>,
    pub other_files: bool,
    /// Paths relative to the mod list's `instances/` for "other_files". D150:
    /// none picked means nothing from the instances.
    #[serde(default)]
    pub selected_other_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveModlistGroupsInput {
    pub modlist_name: String,
    /// Tag definitions (formerly `functionalGroups`).
    #[serde(default, alias = "functionalGroups")]
    pub tags: Vec<PersistedTag>,
    /// Aesthetic groups (visual section containers).
    #[serde(default)]
    pub aesthetic_groups: Vec<PersistedAestheticGroup>,
    /// Row IDs whose alternatives panel is expanded (persisted across reloads).
    #[serde(default)]
    pub collapsed_alts: Vec<String>,
}

#[tauri::command]
pub fn load_modlist_presentation_command(
    launcher_paths: State<'_, LauncherPaths>,
    modlist_name: String,
) -> Result<ModlistPresentation, String> {
    load_modlist_presentation_from_root(launcher_paths.root_dir(), &modlist_name)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn save_modlist_presentation_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: SaveModlistPresentationInput,
) -> Result<(), String> {
    save_modlist_presentation_from_root(launcher_paths.root_dir(), &input)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn load_modlist_groups_command(
    launcher_paths: State<'_, LauncherPaths>,
    modlist_name: String,
) -> Result<ModlistGroupLayout, String> {
    load_modlist_groups_from_root(launcher_paths.root_dir(), &modlist_name)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn save_modlist_groups_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: SaveModlistGroupsInput,
) -> Result<(), String> {
    save_modlist_groups_from_root(launcher_paths.root_dir(), &input)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn export_modlist_command(
    launcher_paths: State<'_, LauncherPaths>,
    input: ExportModlistInput,
) -> Result<(), String> {
    export_modlist_from_root(launcher_paths.root_dir(), &input).map_err(|error| error.to_string())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceFileNode {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<InstanceFileNode>,
}

/// Lists direct children of a directory inside the instances folder.
/// Called with an empty `relative_path` to get instance roots,
/// or with e.g. `"26.1-fabric/config"` to expand that directory.
#[tauri::command]
pub fn list_instance_files_command(
    launcher_paths: State<'_, LauncherPaths>,
    modlist_name: String,
    relative_path: Option<String>,
) -> Result<Vec<InstanceFileNode>, String> {
    list_instance_files_from_root(
        launcher_paths.root_dir(),
        &modlist_name,
        relative_path.as_deref(),
    )
    .map_err(|error| error.to_string())
}

fn list_instance_files_from_root(
    root_dir: &Path,
    modlist_name: &str,
    relative_path: Option<&str>,
) -> Result<Vec<InstanceFileNode>> {
    validate_path_component(modlist_name)?;
    let instances_dir = LauncherPaths::new(root_dir.to_path_buf())
        .modlists_dir()
        .join(modlist_name)
        .join("instances");
    // A mod list never launched has no instances: that is an empty tree, not
    // an error. Anything that fails past this point is said, not turned into
    // an empty list the modal would read as "launch the game first".
    if !instances_dir.exists() {
        return Ok(vec![]);
    }

    let target = match relative_path {
        Some(rel) if !rel.is_empty() => contained_join(&instances_dir, rel)?,
        _ => instances_dir.clone(),
    };

    // Determine depth for filtering (0 = listing instance roots, 1 = inside an instance)
    let depth = match relative_path {
        Some(rel) if !rel.is_empty() => rel.matches('/').count() as u32 + 1,
        _ => 0,
    };

    let mut sorted = Vec::new();
    for entry in
        fs::read_dir(&target).with_context(|| format!("failed to list {}", target.display()))?
    {
        sorted.push(
            entry.with_context(|| format!("failed to read an entry of {}", target.display()))?,
        );
    }
    sorted.sort_by_key(|e| e.file_name());

    let mut nodes = Vec::new();
    for entry in sorted {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        // Skip launcher-managed dirs at instance root level
        if depth == 1
            && matches!(
                name.as_str(),
                "mods" | "libraries" | "natives" | "assets" | "logs" | "data" | "downloads"
            )
        {
            continue;
        }
        let path = entry.path();
        let relative = path.strip_prefix(&instances_dir).unwrap_or(&path);
        let rel_str = relative.to_string_lossy().replace('\\', "/");
        // Through links: a shared world is a link to a folder (D151), and it
        // must open like the folder it is.
        let is_dir = fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false);
        // Children are NOT loaded here — frontend requests them on expand
        nodes.push(InstanceFileNode {
            name,
            path: rel_str,
            is_dir,
            children: vec![],
        });
    }
    Ok(nodes)
}

/// Async and off the main thread (condivisi-7): a synchronous command runs on
/// the GUI thread, and reading and encoding up to 16 MB froze the window for
/// as long as it took.
#[tauri::command]
pub async fn read_image_as_data_url_command(
    launcher_paths: State<'_, LauncherPaths>,
    path: String,
) -> Result<String, String> {
    let root_dir = launcher_paths.root_dir().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || read_image_as_data_url(&root_dir, &path))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

fn read_image_as_data_url(root_dir: &Path, path: &str) -> Result<String> {
    // This reads a user-picked icon. The only caller passes the absolute path
    // returned by the OS file dialog (outside the launcher root), so root
    // containment does not apply here. Restrict the IPC surface to small,
    // regular files whose extension and signature identify a supported image.
    // A launcher-root-relative path is still accepted for callers that supply
    // one.
    const MAX_ICON_BYTES: u64 = 16 * 1024 * 1024;

    let candidate = Path::new(path);
    let resolved = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        contained_join(root_dir, path)?
    };

    let ext = resolved
        .extension()
        .and_then(|extension| extension.to_str())
        .context("image path must have a UTF-8 extension")?;
    let mime = if ext.eq_ignore_ascii_case("png") {
        "image/png"
    } else if ext.eq_ignore_ascii_case("jpg") || ext.eq_ignore_ascii_case("jpeg") {
        "image/jpeg"
    } else if ext.eq_ignore_ascii_case("webp") {
        "image/webp"
    } else if ext.eq_ignore_ascii_case("gif") {
        "image/gif"
    } else if ext.eq_ignore_ascii_case("ico") {
        "image/x-icon"
    } else {
        bail!("unsupported image type for {}", resolved.display());
    };

    let metadata = fs::metadata(&resolved)
        .with_context(|| format!("failed to read image at {}", resolved.display()))?;
    if !metadata.is_file() {
        bail!("image path is not a regular file: {}", resolved.display());
    }
    if metadata.len() > MAX_ICON_BYTES {
        bail!(
            "image at {} is too large ({} bytes; max {})",
            resolved.display(),
            metadata.len(),
            MAX_ICON_BYTES
        );
    }

    let bytes = fs::read(&resolved)
        .with_context(|| format!("failed to read image at {}", resolved.display()))?;
    let has_expected_signature = match mime {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "image/webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP".as_slice()),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/x-icon" => bytes.starts_with(&[0, 0, 1, 0]),
        _ => false,
    };
    if !has_expected_signature {
        bail!(
            "file contents do not match image type for {}",
            resolved.display()
        );
    }

    let b64 = BASE64.encode(&bytes);
    Ok(format!("data:{mime};base64,{b64}"))
}

pub fn load_modlist_presentation_from_root(
    root_dir: &Path,
    modlist_name: &str,
) -> Result<ModlistPresentation> {
    validate_path_component(modlist_name)?;
    // Presentation is stored in the separate modlist-presentation.json file.
    let presentation_path = modlist_presentation_path(root_dir, modlist_name);
    if !presentation_path.exists() {
        return Ok(default_presentation(modlist_name));
    }

    let contents = fs::read_to_string(&presentation_path).with_context(|| {
        format!(
            "failed to read modlist presentation file at {}",
            presentation_path.display()
        )
    })?;

    serde_json::from_str::<ModlistPresentation>(&contents).with_context(|| {
        format!(
            "failed to deserialize modlist presentation file at {}",
            presentation_path.display()
        )
    })
}

pub fn save_modlist_presentation_from_root(
    root_dir: &Path,
    input: &SaveModlistPresentationInput,
) -> Result<()> {
    validate_path_component(&input.modlist_name)?;
    let presentation = ModlistPresentation {
        // A display name equal to the folder name says nothing the folder does
        // not, and it would go stale when an import renames the folder
        // (`r2-ciclo-vita-nome-2`): it is not stored.
        display_name: input
            .display_name
            .clone()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && *s != input.modlist_name),
        icon_label: normalize_icon_label(&input.icon_label, &input.modlist_name),
        icon_accent: input.icon_accent.trim().to_string(),
        notes: input.notes.trim().to_string(),
        icon_image: input.icon_image.clone().filter(|s| !s.is_empty()),
    };

    // Write to the separate modlist-presentation.json file.
    let presentation_path = modlist_presentation_path(root_dir, &input.modlist_name);
    if let Some(parent) = presentation_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(&presentation)
        .with_context(|| "failed to serialize modlist presentation".to_string())?;
    write_atomically(&presentation_path, format!("{json}\n")).with_context(|| {
        format!(
            "failed to write modlist presentation file at {}",
            presentation_path.display()
        )
    })
}

/// Drop the display name of the mod list in `modlist_dir`, so the folder name
/// shows (`r2-ciclo-vita-nome-2`). For an import that had to rename the folder
/// to «Pack (1)»: the display name it carried is the one of the list it was
/// exported from, and keeping it shows two identical «Pack» in the sidebar.
/// No presentation file, or one without a display name, is left as it is.
pub(crate) fn forget_display_name(modlist_dir: &Path) -> Result<()> {
    let path = modlist_dir.join(MODLIST_PRESENTATION_FILENAME);
    if !path.exists() {
        return Ok(());
    }
    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut presentation: ModlistPresentation = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    if presentation.display_name.take().is_none() {
        return Ok(());
    }
    let json = serde_json::to_string_pretty(&presentation)
        .context("failed to serialize modlist presentation")?;
    write_atomically(&path, format!("{json}\n"))
        .with_context(|| format!("failed to write {}", path.display()))
}

pub fn load_modlist_groups_from_root(
    root_dir: &Path,
    modlist_name: &str,
) -> Result<ModlistGroupLayout> {
    validate_path_component(modlist_name)?;
    let layout_path = modlist_group_layout_path(root_dir, modlist_name);
    if !layout_path.exists() {
        return Ok(default_group_layout());
    }

    let contents = fs::read_to_string(&layout_path).with_context(|| {
        format!(
            "failed to read modlist group layout file at {}",
            layout_path.display()
        )
    })?;

    serde_json::from_str::<ModlistGroupLayout>(&contents).with_context(|| {
        format!(
            "failed to deserialize modlist group layout file at {}",
            layout_path.display()
        )
    })
}

pub fn save_modlist_groups_from_root(
    root_dir: &Path,
    input: &SaveModlistGroupsInput,
) -> Result<()> {
    validate_path_component(&input.modlist_name)?;
    let layout = ModlistGroupLayout {
        tags: input.tags.clone(),
        aesthetic_groups: input.aesthetic_groups.clone(),
        collapsed_alts: input.collapsed_alts.clone(),
    };
    let layout_path = modlist_group_layout_path(root_dir, &input.modlist_name);

    if let Some(parent) = layout_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let json = serde_json::to_string_pretty(&layout)
        .with_context(|| "failed to serialize modlist group layout".to_string())?;
    write_atomically(&layout_path, format!("{json}\n")).with_context(|| {
        format!(
            "failed to write modlist group layout file at {}",
            layout_path.display()
        )
    })?;

    Ok(())
}

pub fn export_modlist_from_root(root_dir: &Path, input: &ExportModlistInput) -> Result<()> {
    validate_path_component(&input.modlist_name)?;
    let launcher_paths = LauncherPaths::new(root_dir.to_path_buf());
    let modlist_dir = launcher_paths.modlists_dir().join(&input.modlist_name);
    anyhow::ensure!(
        modlist_dir.exists(),
        "mod-list '{}' does not exist",
        input.modlist_name
    );

    let destination_path = PathBuf::from(input.destination_path.trim());
    anyhow::ensure!(
        !destination_path.as_os_str().is_empty(),
        "destination_path cannot be empty"
    );
    let destination_name = destination_path
        .file_name()
        .with_context(|| format!("{} does not name a file", destination_path.display()))?;
    if let Some(parent) = destination_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    // `concorrenza-7`: lo zip si scrive accanto alla destinazione e prende il
    // suo nome solo dopo un `finish()` riuscito. Un export che fallisce a metà
    // non lascia un archivio rotto dove l'utente l'aspetta, e un file che era
    // già lì resta com'era. Accanto e non in `env::temp_dir()`, perché una
    // rename fra due filesystem fallisce (`atomic_write.rs`).
    let mut partial_name = destination_name.to_os_string();
    partial_name.push(".part");
    let partial_path = destination_path.with_file_name(partial_name);
    let result =
        write_export_archive(root_dir, &modlist_dir, input, &partial_path).and_then(|()| {
            fs::rename(&partial_path, &destination_path).with_context(|| {
                format!(
                    "failed to move the finished archive to {}",
                    destination_path.display()
                )
            })
        });
    if result.is_err() {
        let _ = fs::remove_file(&partial_path);
    }
    result
}

fn write_export_archive(
    root_dir: &Path,
    modlist_dir: &Path,
    input: &ExportModlistInput,
    partial_path: &Path,
) -> Result<()> {
    let file = fs::File::create(partial_path).with_context(|| {
        format!(
            "failed to create export archive at {}",
            partial_path.display()
        )
    })?;
    let mut archive = zip::ZipWriter::new(file);
    let mut added_paths = HashSet::new();
    let archive_root = normalize_archive_root(&input.modlist_name);

    if input.rules_json {
        add_file_if_exists(
            &mut archive,
            &mut added_paths,
            &modlist_dir.join(RULES_FILENAME),
            &format!("{archive_root}/{RULES_FILENAME}"),
        )?;
    }

    add_file_if_exists(
        &mut archive,
        &mut added_paths,
        &modlist_presentation_path(root_dir, &input.modlist_name),
        &format!("{archive_root}/{MODLIST_PRESENTATION_FILENAME}"),
    )?;
    add_file_if_exists(
        &mut archive,
        &mut added_paths,
        &modlist_group_layout_path(root_dir, &input.modlist_name),
        &format!("{archive_root}/{MODLIST_GROUP_LAYOUT_FILENAME}"),
    )?;

    // `modlist-4`: le liste dei content pack sono la definizione della mod
    // list quanto rules.json, quindi viaggiano sempre. Senza, chi importa
    // trova le schede Content vuote e nessun pack viene scaricato.
    for content_type in CONTENT_TYPES {
        let file_name = filename_for_type(content_type);
        add_file_if_exists(
            &mut archive,
            &mut added_paths,
            &modlist_dir.join(file_name),
            &format!("{archive_root}/{file_name}"),
        )?;
    }

    // D107: i jar di `local-jars/` sono gli unici che chi importa non può
    // riscaricare (un mod pinnato è un mod locale, D14), quindi viaggiano
    // sempre, come presentation e group layout. La cache globale dei mod no:
    // è la metà riscaricabile, e l'import scarta comunque tutto `cache/`.
    // Una cartella vuota o assente non aggiunge voci.
    add_directory_contents(
        &mut archive,
        &mut added_paths,
        &modlist_dir.join(LOCAL_JARS_DIR),
        &format!("{archive_root}/{LOCAL_JARS_DIR}"),
    )?;

    // 085 scheda 5: i file dei custom config viaggiano come stanno nella mod
    // list, `config-files/<mod_id>/<id>/<nome>`, cioè dove l'import li rimette
    // e il lancio li cerca. Nessuna scelta fra due file con lo stesso nome qui:
    // la fa il lancio, la stessa da chi esporta e da chi importa
    // (todo.md:1295). La vecchia `cache/configs` globale non c'è più: era
    // sempre vuota, e l'import la scartava.
    if input.config_files {
        add_directory_contents(
            &mut archive,
            &mut added_paths,
            &modlist_dir.join(CUSTOM_CONFIG_FILES_DIRNAME),
            &format!("{archive_root}/{CUSTOM_CONFIG_FILES_DIRNAME}"),
        )?;
    }

    // `modlist-4`, D150: i pack locali escono dalle cartelle di categoria della
    // mod list, mai dalle copie viste attraverso i link delle istanze. Una
    // categoria spuntata con dei pack scelti uno per uno porta solo quelli;
    // senza scelte, tutta la cartella.
    let picked_packs = picked_content_packs(&input.selected_content_paths)?;
    for (enabled, content_type) in [
        (input.resource_packs, "resourcepack"),
        (input.data_packs, "datapack"),
        (input.shaders, "shader"),
    ] {
        let Some(category_dir) = modlist_category_dir(content_type) else {
            continue;
        };
        if !enabled {
            continue;
        }
        let source_dir = modlist_dir.join(category_dir);
        let names = match picked_packs.get(category_dir) {
            Some(names) => names.clone(),
            None => child_names(&source_dir)?,
        };
        for name in names {
            let source = source_dir.join(&name);
            anyhow::ensure!(
                fs::symlink_metadata(&source).is_ok(),
                "the pack '{name}' is not in the mod list's {category_dir} folder"
            );
            add_path_contents(
                &mut archive,
                &mut added_paths,
                &source,
                &format!("{archive_root}/{category_dir}/{name}"),
                |_| true,
            )?;
            let icon = icon_relative_path(category_dir, &name);
            add_file_if_exists(
                &mut archive,
                &mut added_paths,
                &contained_join(modlist_dir, &icon)?,
                &format!("{archive_root}/{icon}"),
            )?;
        }
    }

    // D150: «Other files» porta solo i percorsi scelti nell'albero delle
    // istanze; senza scelte, niente. Prima, senza scelte usciva tutta la
    // cartella della mod list, filtrata guardando il percorso assoluto
    // (`windows-7`: una mod list chiamata «Minecraft» non esportava niente).
    if input.other_files {
        let instances_dir = modlist_dir.join("instances");
        for selected in &input.selected_other_paths {
            let full_path = contained_join(&instances_dir, selected)?;
            let relative = full_path
                .strip_prefix(&instances_dir)
                .map(path_to_archive_string)
                .unwrap_or_default();
            anyhow::ensure!(
                !relative.is_empty(),
                "pick a file or a folder inside an instance, not '{selected}'"
            );
            // D77: la cronologia comandi non esce da qui, e non c'è
            // un'opzione per rimetterla. Il gioco scrive in
            // `command_history.txt` ogni riga di chat che comincia per `/`,
            // testo esatto e senza redazione, quindi il `/login <password>`
            // con cui si entra nei server AuthMe finisce lì in chiaro — e un
            // archivio di modlist è una cosa che si manda a qualcun altro.
            // Vale anche per il file nominato esplicitamente e per la cartella
            // che lo contiene: è proprio il ramo che qualcuno userebbe per
            // aggirarlo. Togliere questo filtro è una perdita di credenziali,
            // non una semplificazione.
            add_path_contents(
                &mut archive,
                &mut added_paths,
                &full_path,
                &format!("{archive_root}/instances/{relative}"),
                |path| {
                    !crate::shared_files::is_excluded_from_export(
                        &path.file_name().unwrap_or_default().to_string_lossy(),
                    )
                },
            )?;
        }
    }

    let file = archive
        .finish()
        .with_context(|| format!("failed to finalize {}", partial_path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to flush {}", partial_path.display()))?;
    Ok(())
}

/// The content types whose list (`resourcepacks.json`, …) is part of a mod
/// list's definition.
const CONTENT_TYPES: [&str; 3] = ["resourcepack", "datapack", "shader"];

/// D150: the packs picked one by one, grouped by category folder. Each path
/// is `<category folder>/<pack>`, relative to the mod list; anything else is
/// refused, so a picked path never reads outside the category folders.
fn picked_content_packs(selected: &[String]) -> Result<HashMap<&'static str, Vec<String>>> {
    let mut picked: HashMap<&'static str, Vec<String>> = HashMap::new();
    for path in selected {
        let unified = path.replace('\\', "/");
        let category = CONTENT_TYPES
            .iter()
            .filter_map(|content_type| modlist_category_dir(content_type))
            .find(|dir| unified.starts_with(&format!("{dir}/")));
        let Some(category) = category else {
            bail!("'{path}' is not a pack of the mod list");
        };
        let name = &unified[category.len() + 1..];
        validate_path_component(name)
            .with_context(|| format!("'{path}' is not a pack of the mod list"))?;
        let names = picked.entry(category).or_default();
        if !names.iter().any(|known| known == name) {
            names.push(name.to_string());
        }
    }
    Ok(picked)
}

/// The names in a directory, sorted; a directory that is not there has none.
fn child_names(dir: &Path) -> Result<Vec<String>> {
    let read_dir = match fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", dir.display()))
        }
    };
    let mut names = Vec::new();
    for entry in read_dir {
        let entry =
            entry.with_context(|| format!("failed to read an entry of {}", dir.display()))?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(names)
}

fn modlist_presentation_path(root_dir: &Path, modlist_name: &str) -> PathBuf {
    LauncherPaths::new(root_dir.to_path_buf())
        .modlists_dir()
        .join(modlist_name)
        .join(MODLIST_PRESENTATION_FILENAME)
}

fn modlist_group_layout_path(root_dir: &Path, modlist_name: &str) -> PathBuf {
    LauncherPaths::new(root_dir.to_path_buf())
        .modlists_dir()
        .join(modlist_name)
        .join(MODLIST_GROUP_LAYOUT_FILENAME)
}

fn default_presentation(modlist_name: &str) -> ModlistPresentation {
    ModlistPresentation {
        display_name: None,
        icon_label: normalize_icon_label("", modlist_name),
        icon_accent: String::new(),
        notes: String::new(),
        icon_image: None,
    }
}

fn default_group_layout() -> ModlistGroupLayout {
    ModlistGroupLayout {
        tags: Vec::new(),
        aesthetic_groups: Vec::new(),
        collapsed_alts: Vec::new(),
    }
}

fn normalize_icon_label(icon_label: &str, modlist_name: &str) -> String {
    let trimmed = icon_label.trim();
    if !trimmed.is_empty() {
        return trimmed.chars().take(3).collect::<String>().to_uppercase();
    }

    let collected = modlist_name
        .split_whitespace()
        .filter_map(|segment| {
            segment
                .chars()
                .find(|character| character.is_alphanumeric())
        })
        .take(3)
        .collect::<String>();

    if collected.is_empty() {
        "ML".to_string()
    } else {
        collected.to_uppercase()
    }
}

fn normalize_archive_root(modlist_name: &str) -> String {
    let sanitized = modlist_name
        .trim()
        .chars()
        .map(|character| match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            other => other,
        })
        .collect::<String>();

    if sanitized.is_empty() {
        "modlist-export".to_string()
    } else {
        sanitized
    }
}

/// Every regular file under `source` (or `source` itself, when it is a file),
/// with its path relative to `source` written with `/`; empty for `source`
/// itself.
///
/// Links are followed. D151: a shared world is a link to a folder in
/// `saves/` (a symlink on Linux, a junction on Windows), and it goes into the
/// archive as the world it is — before, it was read as a file and the export
/// failed with "Is a directory" (todo.md:1574). A folder that is its own
/// ancestor through a link is walked once, and a link that points at nothing
/// is skipped with a line in the log: neither has anything to export.
fn collect_files_following_links(source: &Path) -> Result<Vec<(PathBuf, String)>> {
    fn walk(
        path: &Path,
        relative: String,
        ancestors: &mut Vec<PathBuf>,
        files: &mut Vec<(PathBuf, String)>,
    ) -> Result<()> {
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && fs::symlink_metadata(path).is_ok() =>
            {
                eprintln!(
                    "[export] skipped {}: it is a link to something that is not there",
                    path.display()
                );
                return Ok(());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()))
            }
        };

        if metadata.is_file() {
            files.push((path.to_path_buf(), relative));
            return Ok(());
        }
        if !metadata.is_dir() {
            return Ok(());
        }

        let canonical = fs::canonicalize(path)
            .with_context(|| format!("failed to resolve {}", path.display()))?;
        if ancestors.contains(&canonical) {
            eprintln!(
                "[export] skipped {}: it links back to a folder that contains it",
                path.display()
            );
            return Ok(());
        }
        ancestors.push(canonical);
        let mut entries = Vec::new();
        for entry in
            fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))?
        {
            entries.push(
                entry.with_context(|| format!("failed to read an entry of {}", path.display()))?,
            );
        }
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            let child_relative = if relative.is_empty() {
                name
            } else {
                format!("{relative}/{name}")
            };
            walk(&entry.path(), child_relative, ancestors, files)?;
        }
        ancestors.pop();
        Ok(())
    }

    let mut files = Vec::new();
    walk(source, String::new(), &mut Vec::new(), &mut files)?;
    Ok(files)
}

/// Add `source` — a file, or a folder and everything in it — under
/// `archive_path`, keeping only the files `keep` accepts. A `source` that is
/// not there adds nothing.
fn add_path_contents<W: Write + Seek>(
    archive: &mut zip::ZipWriter<W>,
    added_paths: &mut HashSet<String>,
    source: &Path,
    archive_path: &str,
    keep: impl Fn(&Path) -> bool,
) -> Result<()> {
    if fs::symlink_metadata(source).is_err() {
        return Ok(());
    }
    for (path, relative) in collect_files_following_links(source)? {
        if !keep(&path) {
            continue;
        }
        let entry = if relative.is_empty() {
            archive_path.to_string()
        } else {
            format!("{archive_path}/{relative}")
        };
        add_file_to_zip(archive, added_paths, &path, &entry)?;
    }
    Ok(())
}

fn add_directory_contents<W: Write + Seek>(
    archive: &mut zip::ZipWriter<W>,
    added_paths: &mut HashSet<String>,
    source_root: &Path,
    archive_root: &str,
) -> Result<()> {
    add_path_contents(archive, added_paths, source_root, archive_root, |_| true)
}

fn add_file_if_exists<W: Write + Seek>(
    archive: &mut zip::ZipWriter<W>,
    added_paths: &mut HashSet<String>,
    source_path: &Path,
    archive_path: &str,
) -> Result<()> {
    if !source_path.exists() {
        return Ok(());
    }

    add_file_to_zip(archive, added_paths, source_path, archive_path)
}

fn add_file_to_zip<W: Write + Seek>(
    archive: &mut zip::ZipWriter<W>,
    added_paths: &mut HashSet<String>,
    source_path: &Path,
    archive_path: &str,
) -> Result<()> {
    if !added_paths.insert(archive_path.to_string()) {
        return Ok(());
    }

    let mut source_file = fs::File::open(source_path)
        .with_context(|| format!("failed to open {}", source_path.display()))?;

    archive
        .start_file(
            archive_path,
            FileOptions::default().compression_method(zip::CompressionMethod::Deflated),
        )
        .with_context(|| format!("failed to create archive entry {archive_path}"))?;
    // A file at a time, not read whole into memory: a world can be gigabytes.
    io::copy(&mut source_file, archive).with_context(|| {
        format!(
            "failed to write archive entry {archive_path} from {}",
            source_path.display()
        )
    })?;

    Ok(())
}

fn path_to_archive_string(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::io::Read;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        export_modlist_from_root, list_instance_files_from_root, load_modlist_groups_from_root,
        load_modlist_presentation_from_root, read_image_as_data_url, save_modlist_groups_from_root,
        save_modlist_presentation_from_root, ExportModlistInput, ModlistGroupLayout, PersistedTag,
        SaveModlistGroupsInput, SaveModlistPresentationInput, MODLIST_GROUP_LAYOUT_FILENAME,
        MODLIST_PRESENTATION_FILENAME,
    };
    use crate::rules::{ModlistPresentation, RULES_FILENAME};

    fn unique_test_root() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        env::temp_dir().join(format!("cubic-launcher-assets-test-{timestamp}"))
    }

    #[test]
    fn load_presentation_returns_defaults_when_file_is_missing() {
        let root_dir = unique_test_root();
        fs::create_dir_all(root_dir.join("mod-lists").join("My Pack"))
            .expect("modlist directory should exist");

        let presentation =
            load_modlist_presentation_from_root(&root_dir, "My Pack").expect("should load");

        assert_eq!(presentation.icon_label, "MP");
        assert_eq!(presentation.icon_accent, "");
        assert_eq!(presentation.notes, "");

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn save_and_load_presentation_roundtrip() {
        let root_dir = unique_test_root();
        let input = SaveModlistPresentationInput {
            modlist_name: "Sky Pack".into(),
            display_name: None,
            icon_label: "sp".into(),
            icon_accent: "Aurora".into(),
            notes: "Bring shaders and minimap.".into(),
            icon_image: None,
        };

        save_modlist_presentation_from_root(&root_dir, &input).expect("presentation should save");
        let reloaded =
            load_modlist_presentation_from_root(&root_dir, "Sky Pack").expect("should load");

        assert_eq!(
            reloaded,
            ModlistPresentation {
                display_name: None,
                icon_label: "SP".into(),
                icon_accent: "Aurora".into(),
                notes: "Bring shaders and minimap.".into(),
                icon_image: None,
            }
        );

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn load_group_layout_returns_empty_defaults_when_file_is_missing() {
        let root_dir = unique_test_root();
        fs::create_dir_all(root_dir.join("mod-lists").join("My Pack"))
            .expect("modlist directory should exist");

        let layout = load_modlist_groups_from_root(&root_dir, "My Pack").expect("should load");

        assert_eq!(
            layout,
            ModlistGroupLayout {
                tags: Vec::new(),
                aesthetic_groups: Vec::new(),
                collapsed_alts: Vec::new(),
            }
        );

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn save_and_load_group_layout_roundtrip() {
        let root_dir = unique_test_root();
        let input = SaveModlistGroupsInput {
            modlist_name: "Sky Pack".into(),
            tags: vec![PersistedTag {
                id: "tag-1".into(),
                name: "Performance".into(),
                tone: "violet".into(),
                mod_ids: vec!["rule-0-sodium".into()],
            }],
            aesthetic_groups: Vec::new(),
            collapsed_alts: Vec::new(),
        };

        save_modlist_groups_from_root(&root_dir, &input).expect("groups should save");
        let reloaded = load_modlist_groups_from_root(&root_dir, "Sky Pack").expect("should load");

        assert_eq!(
            reloaded,
            ModlistGroupLayout {
                tags: input.tags,
                aesthetic_groups: input.aesthetic_groups,
                collapsed_alts: input.collapsed_alts,
            }
        );

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn list_assets_rejects_traversal_relative_path() {
        let root_dir = unique_test_root();
        let instance_dir = root_dir
            .join("mod-lists")
            .join("Safe Pack")
            .join("instances")
            .join("1.21-fabric");
        fs::create_dir_all(&instance_dir).expect("instance directory should exist");
        fs::write(instance_dir.join("options.txt"), b"safe").expect("instance file should exist");

        assert!(
            list_instance_files_from_root(&root_dir, "Safe Pack", Some("../../etc")).is_err(),
            "a relative path escaping the instances directory must be rejected"
        );
        assert!(
            list_instance_files_from_root(&root_dir, "../Unsafe Pack", None).is_err(),
            "a mod-list name containing a separator must be rejected"
        );

        let nodes = list_instance_files_from_root(&root_dir, "Safe Pack", Some("1.21-fabric"))
            .expect("a contained relative path should be listed");
        assert!(nodes.iter().any(|node| node.name == "options.txt"));

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn export_rejects_traversal_selected_path() {
        let root_dir = unique_test_root();
        let instances_dir = root_dir
            .join("mod-lists")
            .join("Safe Pack")
            .join("instances");
        fs::create_dir_all(instances_dir.join("profile")).expect("instance directory should exist");
        fs::write(instances_dir.join("profile").join("options.txt"), b"safe")
            .expect("selected file should exist");

        let traversal_result = export_modlist_from_root(
            &root_dir,
            &ExportModlistInput {
                modlist_name: "Safe Pack".into(),
                destination_path: root_dir.join("traversal.zip").display().to_string(),
                rules_json: false,
                config_files: false,
                resource_packs: false,
                data_packs: false,
                shaders: false,
                other_files: true,
                selected_content_paths: Vec::new(),
                selected_other_paths: vec!["../../etc/passwd".into()],
            },
        );
        assert!(
            traversal_result.is_err(),
            "a selected path escaping the instances directory must be rejected"
        );

        let valid_archive_path = root_dir.join("valid.zip");
        export_modlist_from_root(
            &root_dir,
            &ExportModlistInput {
                modlist_name: "Safe Pack".into(),
                destination_path: valid_archive_path.display().to_string(),
                rules_json: false,
                config_files: false,
                resource_packs: false,
                data_packs: false,
                shaders: false,
                other_files: true,
                selected_content_paths: Vec::new(),
                selected_other_paths: vec!["profile/options.txt".into()],
            },
        )
        .expect("a contained selected path should export");

        let archive_file =
            fs::File::open(&valid_archive_path).expect("valid export archive should open");
        let mut archive = zip::ZipArchive::new(archive_file).expect("archive should parse");
        assert!(
            archive
                .by_name("Safe Pack/instances/profile/options.txt")
                .is_ok(),
            "the contained selected file should be exported"
        );

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    /// D77. Il test morde da tutt'e due i lati: se qualcuno toglie il filtro
    /// dal ramo "tutto" o da quello "file scelti a mano", uno dei due
    /// `assert!` fallisce e dice perché.
    #[test]
    fn the_command_history_never_leaves_in_an_archive() {
        let root_dir = unique_test_root();
        let instances_dir = root_dir
            .join("mod-lists")
            .join("Safe Pack")
            .join("instances");
        let profile_dir = instances_dir.join("profile");
        fs::create_dir_all(&profile_dir).expect("instance directory should exist");
        fs::write(profile_dir.join("options.txt"), b"fov:70").expect("options should exist");
        fs::write(profile_dir.join("servers.dat"), b"nbt").expect("servers should exist");
        fs::write(
            profile_dir.join("command_history.txt"),
            b"/login hunter2\n/tp 100 64 -200\n",
        )
        .expect("the command history should exist");

        // I punti in cui il filtro deve mordere: il ramo che nomina il file,
        // e quello che nomina la cartella che lo contiene. Il secondo è quello
        // che si può togliere restando verdi, se non lo si prova. Il ramo che
        // rastrellava tutto senza una scelta non c'è più (D150), ma il caso
        // resta: senza scelte non deve uscire niente.
        for (tag, selected) in [
            ("none", Vec::new()),
            ("file", vec!["profile/command_history.txt".to_string()]),
            ("dir", vec!["profile".to_string()]),
        ] {
            let archive_path = root_dir.join(format!("export-{tag}.zip"));
            export_modlist_from_root(
                &root_dir,
                &ExportModlistInput {
                    modlist_name: "Safe Pack".into(),
                    destination_path: archive_path.display().to_string(),
                    rules_json: false,
                    config_files: false,
                    resource_packs: false,
                    data_packs: false,
                    shaders: false,
                    other_files: true,
                    selected_content_paths: Vec::new(),
                    selected_other_paths: selected.clone(),
                },
            )
            .expect("export should succeed");

            let archive_file = fs::File::open(&archive_path).expect("archive should open");
            let archive = zip::ZipArchive::new(archive_file).expect("archive should parse");
            let names: Vec<String> = archive.file_names().map(str::to_string).collect();

            assert!(
                !names
                    .iter()
                    .any(|name| name.ends_with("command_history.txt")),
                "the command history must never be exported (selected: {selected:?}); \
                 the archive held {names:?}"
            );
            // Controprova: il filtro deve essere su un file, non un modo per
            // spegnere il ramo. Nel caso "file" l'utente ha nominato solo la
            // cronologia, e nel caso "none" niente, quindi lì non c'è niente
            // da controllare.
            if tag == "none" {
                assert!(
                    !names.iter().any(|name| name.contains("/instances/")),
                    "D150: «Other files» with nothing picked exports nothing; the archive held {names:?}"
                );
            }
            if tag == "dir" {
                assert!(
                    names
                        .iter()
                        .any(|name| name.ends_with("instances/profile/servers.dat")),
                    "the filter must be about one file, not about switching the branch off \
                     ({tag}); the archive held {names:?}"
                );
            }
        }

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn export_rejects_traversal_modlist_name() {
        let root_dir = unique_test_root();
        let result = export_modlist_from_root(
            &root_dir,
            &ExportModlistInput {
                modlist_name: "../Unsafe Pack".into(),
                destination_path: root_dir.join("unsafe.zip").display().to_string(),
                rules_json: false,
                config_files: false,
                resource_packs: false,
                data_packs: false,
                shaders: false,
                other_files: false,
                selected_content_paths: Vec::new(),
                selected_other_paths: Vec::new(),
            },
        );

        assert!(
            result.is_err(),
            "a traversal mod-list name must be rejected"
        );
        assert!(
            !root_dir.join("unsafe.zip").exists(),
            "validation must happen before creating the archive"
        );
    }

    #[test]
    fn presentation_rejects_traversal_modlist_name() {
        let root_dir = unique_test_root();
        assert!(
            load_modlist_presentation_from_root(&root_dir, "..").is_err(),
            "loading must reject a traversal mod-list name"
        );

        let input = SaveModlistPresentationInput {
            modlist_name: "..".into(),
            display_name: None,
            icon_label: "x".into(),
            icon_accent: String::new(),
            notes: String::new(),
            icon_image: None,
        };
        assert!(
            save_modlist_presentation_from_root(&root_dir, &input).is_err(),
            "saving must reject a traversal mod-list name"
        );
    }

    #[test]
    fn groups_reject_traversal_modlist_name() {
        let root_dir = unique_test_root();
        assert!(
            load_modlist_groups_from_root(&root_dir, "..").is_err(),
            "loading must reject a traversal mod-list name"
        );

        let input = SaveModlistGroupsInput {
            modlist_name: "..".into(),
            tags: Vec::new(),
            aesthetic_groups: Vec::new(),
            collapsed_alts: Vec::new(),
        };
        assert!(
            save_modlist_groups_from_root(&root_dir, &input).is_err(),
            "saving must reject a traversal mod-list name"
        );
    }

    #[test]
    fn read_image_rejects_escape_and_reads_root_relative_path() {
        let root_dir = unique_test_root();
        let image_path = root_dir.join("icons").join("icon.png");
        fs::create_dir_all(image_path.parent().expect("image should have a parent"))
            .expect("image directory should exist");
        fs::write(&image_path, b"\x89PNG\r\n\x1a\nimage").expect("image should exist");

        // Root-relative traversal is still rejected (contained_join path).
        assert!(
            read_image_as_data_url(&root_dir, "../outside.png").is_err(),
            "a relative image path escaping the launcher root must be rejected"
        );
        let data_url = read_image_as_data_url(&root_dir, "icons/icon.png")
            .expect("a launcher-root-relative image should be read");
        assert!(data_url.starts_with("data:image/png;base64,"));

        // The real caller passes the OS dialog's ABSOLUTE path, outside the
        // launcher root: it must be accepted when it carries an image extension.
        let outside = unique_test_root();
        let outside_icon = outside.join("Pictures").join("avatar.jpeg");
        fs::create_dir_all(outside_icon.parent().expect("parent")).expect("dir");
        fs::write(&outside_icon, b"\xff\xd8\xffjpegbytes").expect("write");
        let abs_url = read_image_as_data_url(&root_dir, &outside_icon.to_string_lossy())
            .expect("an absolute dialog image path must be accepted");
        assert!(abs_url.starts_with("data:image/jpeg;base64,"));

        // A non-image absolute path (exfiltration attempt) is rejected on ext.
        let secret = outside.join("secret.txt");
        fs::write(&secret, b"top secret").expect("write");
        assert!(
            read_image_as_data_url(&root_dir, &secret.to_string_lossy()).is_err(),
            "a non-image absolute path must be rejected by the extension guard"
        );
        let disguised_secret = outside.join("secret.png");
        fs::write(&disguised_secret, b"top secret").expect("write");
        assert!(
            read_image_as_data_url(&root_dir, &disguised_secret.to_string_lossy()).is_err(),
            "an image extension without a matching signature must be rejected"
        );

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
        fs::remove_dir_all(&outside).expect("temporary outside dir should be removable");
    }

    fn export_input(modlist_name: &str, destination: &std::path::Path) -> ExportModlistInput {
        ExportModlistInput {
            modlist_name: modlist_name.into(),
            destination_path: destination.display().to_string(),
            rules_json: true,
            config_files: false,
            resource_packs: false,
            data_packs: false,
            shaders: false,
            selected_content_paths: Vec::new(),
            other_files: false,
            selected_other_paths: Vec::new(),
        }
    }

    fn write_file(path: &std::path::Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().expect("file should have a parent"))
            .expect("parent should be created");
        fs::write(path, bytes).expect("file should be written");
    }

    /// `modlist-4`: the three pack lists travel always, like rules.json, and
    /// a checked category carries the mod list's own folder — never the
    /// copies seen through an instance's links, and never the old global
    /// `cache/configs` (085 scheda 3, punto 7).
    #[test]
    fn export_archive_includes_rules_presentation_and_selected_assets() {
        let root_dir = unique_test_root();
        let modlist_dir = root_dir.join("mod-lists").join("Sky Pack");
        let export_path = root_dir.join("sky-pack.zip");

        write_file(&modlist_dir.join(RULES_FILENAME), b"{}\n");
        write_file(
            &modlist_dir.join(MODLIST_PRESENTATION_FILENAME),
            b"{\n  \"iconLabel\": \"SP\",\n  \"iconAccent\": \"Sky\",\n  \"notes\": \"Bring elytra\"\n}\n",
        );
        write_file(
            &modlist_dir.join(MODLIST_GROUP_LAYOUT_FILENAME),
            b"{\n  \"aestheticGroups\": [{\"id\": \"ag-1\", \"name\": \"Visuals\", \"collapsed\": false}],\n  \"tags\": []\n}\n",
        );
        for list in ["resourcepacks.json", "datapacks.json", "shaders.json"] {
            write_file(&modlist_dir.join(list), list.as_bytes());
        }
        write_file(&modlist_dir.join("resourcepacks").join("sky.zip"), b"pack");
        write_file(
            &modlist_dir
                .join("resourcepacks")
                .join("Unpacked Pack")
                .join("pack.mcmeta"),
            b"{}",
        );
        write_file(
            &modlist_dir
                .join(".cubic")
                .join("icons")
                .join("resourcepacks")
                .join("sky.zip.png"),
            b"\x89PNG\r\n\x1a\nicon",
        );
        write_file(&modlist_dir.join("shaders").join("bsl.zip"), b"shader");
        write_file(
            &modlist_dir
                .join("instances")
                .join("1.21.1-fabric")
                .join("resourcepacks")
                .join("sky.zip"),
            b"instance copy",
        );
        write_file(
            &root_dir
                .join("cache")
                .join("configs")
                .join("sodium")
                .join("options.json"),
            b"config",
        );

        let mut input = export_input("Sky Pack", &export_path);
        input.config_files = true;
        input.resource_packs = true;
        export_modlist_from_root(&root_dir, &input).expect("export should succeed");

        let names = archive_names(&export_path);
        for expected in [
            "Sky Pack/rules.json",
            "Sky Pack/modlist-presentation.json",
            "Sky Pack/modlist-editor-groups.json",
            "Sky Pack/resourcepacks.json",
            "Sky Pack/datapacks.json",
            "Sky Pack/shaders.json",
            "Sky Pack/resourcepacks/sky.zip",
            "Sky Pack/resourcepacks/Unpacked Pack/pack.mcmeta",
            "Sky Pack/.cubic/icons/resourcepacks/sky.zip.png",
        ] {
            assert!(
                names.contains(&expected.to_string()),
                "{expected} must travel; the archive held {names:?}"
            );
        }
        for absent in [
            "Sky Pack/shaders/bsl.zip",
            "Sky Pack/instances/1.21.1-fabric/resourcepacks/sky.zip",
            "Sky Pack/cache/configs/sodium/options.json",
        ] {
            assert!(
                !names.contains(&absent.to_string()),
                "{absent} must not travel; the archive held {names:?}"
            );
        }

        let archive_file = fs::File::open(&export_path).expect("archive should open");
        let mut archive = zip::ZipArchive::new(archive_file).expect("archive should parse");
        let mut presentation = String::new();
        archive
            .by_name(&format!("Sky Pack/{MODLIST_PRESENTATION_FILENAME}"))
            .expect("presentation entry should exist")
            .read_to_string(&mut presentation)
            .expect("presentation should read");
        assert!(presentation.contains("Bring elytra"));
        let mut pack = Vec::new();
        archive
            .by_name("Sky Pack/resourcepacks/sky.zip")
            .expect("pack entry should exist")
            .read_to_end(&mut pack)
            .expect("pack should read");
        assert_eq!(pack, b"pack", "the mod list's own copy, not the instance's");

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    /// D150: the packs picked one by one filter their category; a category
    /// left unchecked exports nothing even with picks; a pick outside the
    /// category folders is refused.
    #[test]
    fn picked_packs_filter_their_category() {
        let root_dir = unique_test_root();
        let modlist_dir = root_dir.join("mod-lists").join("Pack");
        write_file(&modlist_dir.join(RULES_FILENAME), b"{}\n");
        write_file(&modlist_dir.join("resourcepacks").join("a.zip"), b"a");
        write_file(&modlist_dir.join("resourcepacks").join("b.zip"), b"b");
        write_file(&modlist_dir.join("shaders").join("s.zip"), b"s");

        let export_path = root_dir.join("pack.zip");
        let mut input = export_input("Pack", &export_path);
        input.resource_packs = true;
        input.selected_content_paths = vec!["resourcepacks/b.zip".into(), "shaders/s.zip".into()];
        export_modlist_from_root(&root_dir, &input).expect("export should succeed");

        let names = archive_names(&export_path);
        assert!(
            names.contains(&"Pack/resourcepacks/b.zip".to_string()),
            "{names:?}"
        );
        assert!(
            !names.contains(&"Pack/resourcepacks/a.zip".to_string()),
            "{names:?}"
        );
        assert!(
            !names.contains(&"Pack/shaders/s.zip".to_string()),
            "an unchecked category exports nothing, picks or not: {names:?}"
        );

        for outside in [
            "rules.json",
            "resourcepacks/../rules.json",
            "instances/x/options.txt",
        ] {
            let mut input = export_input("Pack", &root_dir.join("bad.zip"));
            input.resource_packs = true;
            input.selected_content_paths = vec![outside.into()];
            assert!(
                export_modlist_from_root(&root_dir, &input).is_err(),
                "{outside} is not a pack and must be refused"
            );
        }

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    /// `windows-7`: what is exported is decided on paths relative to the mod
    /// list, so a mod list whose name is also a folder name the export
    /// handles specially exports the same as any other.
    #[test]
    fn a_mod_list_named_like_a_managed_folder_exports_normally() {
        for name in ["Minecraft", "resourcepacks", "config"] {
            let root_dir = unique_test_root();
            let modlist_dir = root_dir.join("mod-lists").join(name);
            write_file(&modlist_dir.join(RULES_FILENAME), b"{}\n");
            write_file(&modlist_dir.join("resourcepacks").join("a.zip"), b"a");
            write_file(
                &modlist_dir
                    .join("instances")
                    .join("1.21.1-fabric")
                    .join("options.txt"),
                b"fov:70",
            );

            let export_path = root_dir.join("export.zip");
            let mut input = export_input(name, &export_path);
            input.other_files = true;
            input.selected_other_paths = vec!["1.21.1-fabric/options.txt".into()];
            export_modlist_from_root(&root_dir, &input).expect("export should succeed");

            let names = archive_names(&export_path);
            assert!(
                names.contains(&format!("{name}/instances/1.21.1-fabric/options.txt")),
                "{name}: {names:?}"
            );
            assert!(
                !names.contains(&format!("{name}/resourcepacks/a.zip")),
                "{name}: the category was not checked: {names:?}"
            );
            fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
        }
    }

    /// D151, `concorrenza-7`, todo.md:1574: a shared world is a link to a
    /// folder in `saves/`, and the export follows it — before, it failed with
    /// "Is a directory". A link back to an ancestor and a link to nothing do
    /// not stop the export.
    #[cfg(unix)]
    #[test]
    fn a_shared_world_link_is_followed_into_the_archive() {
        use std::os::unix::fs::symlink;

        let root_dir = unique_test_root();
        let modlist_dir = root_dir.join("mod-lists").join("Pack");
        write_file(&modlist_dir.join(RULES_FILENAME), b"{}\n");
        let world = root_dir.join("worlds").join("New World");
        write_file(&world.join("level.dat"), b"level");
        write_file(&world.join("region").join("r.0.0.mca"), b"region");
        let saves = modlist_dir
            .join("instances")
            .join("1.21.1-fabric")
            .join("saves");
        fs::create_dir_all(&saves).unwrap();
        symlink(&world, saves.join("New World shared")).unwrap();
        symlink(&saves, saves.join("loop")).unwrap();
        symlink(root_dir.join("nowhere"), saves.join("dangling")).unwrap();

        let export_path = root_dir.join("pack.zip");
        let mut input = export_input("Pack", &export_path);
        input.other_files = true;
        input.selected_other_paths = vec!["1.21.1-fabric/saves".into()];
        export_modlist_from_root(&root_dir, &input).expect("export should succeed");

        let names = archive_names(&export_path);
        for expected in [
            "Pack/instances/1.21.1-fabric/saves/New World shared/level.dat",
            "Pack/instances/1.21.1-fabric/saves/New World shared/region/r.0.0.mca",
        ] {
            assert!(
                names.contains(&expected.to_string()),
                "{expected}: {names:?}"
            );
        }
        assert!(
            !names.iter().any(|name| name.contains("/loop/")),
            "a link back to an ancestor is walked once: {names:?}"
        );

        let nodes = list_instance_files_from_root(&root_dir, "Pack", Some("1.21.1-fabric/saves"))
            .expect("the saves folder should list");
        let shared = nodes
            .iter()
            .find(|node| node.name == "New World shared")
            .expect("the shared world should be listed");
        assert!(shared.is_dir, "a shared world opens like a folder");

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    /// `concorrenza-7`: the archive is written beside the destination and
    /// renamed at the end, so a failed export leaves no broken zip and does
    /// not touch a file already there.
    #[test]
    fn a_failed_export_leaves_the_destination_as_it_was() {
        let root_dir = unique_test_root();
        let modlist_dir = root_dir.join("mod-lists").join("Pack");
        write_file(&modlist_dir.join(RULES_FILENAME), b"{}\n");
        let export_path = root_dir.join("out").join("pack.zip");
        write_file(&export_path, b"the previous export");

        let mut input = export_input("Pack", &export_path);
        input.resource_packs = true;
        input.selected_content_paths = vec!["resourcepacks/missing.zip".into()];
        let error = export_modlist_from_root(&root_dir, &input)
            .expect_err("a picked pack that is not there must fail the export");
        assert!(format!("{error:#}").contains("missing.zip"), "{error:#}");

        assert_eq!(fs::read(&export_path).unwrap(), b"the previous export");
        let leftovers: Vec<_> = fs::read_dir(root_dir.join("out"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("pack.zip")]);

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    /// Listing the instance tree says why it failed instead of answering
    /// with an empty list the modal would show as "launch the game first".
    #[test]
    fn listing_a_folder_that_is_not_there_is_an_error() {
        let root_dir = unique_test_root();
        let instance = root_dir
            .join("mod-lists")
            .join("Pack")
            .join("instances")
            .join("1.21.1-fabric");
        fs::create_dir_all(&instance).unwrap();

        assert!(
            list_instance_files_from_root(&root_dir, "Pack", Some("1.21.1-fabric/saves")).is_err()
        );
        assert!(
            list_instance_files_from_root(&root_dir, "Never Launched", None)
                .expect("a mod list without instances has an empty tree")
                .is_empty()
        );

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    /// Mod list «Pack» con una regola locale e il suo jar in `local-jars/`.
    /// Ritorna i byte del jar, che sono l'unica copia: nessuno li può
    /// riscaricare.
    fn write_modlist_with_local_mod(root_dir: &std::path::Path) -> Vec<u8> {
        use crate::rules::{ModList, ModSource, Rule};

        let modlist_dir = root_dir.join("mod-lists").join("Pack");
        let local_jars_dir = modlist_dir.join("local-jars");
        fs::create_dir_all(&local_jars_dir).expect("local-jars dir should exist");
        ModList {
            modlist_name: "Pack".into(),
            author: "Author".into(),
            description: String::new(),
            rules: vec![Rule {
                mod_id: "custom-patch".into(),
                source: ModSource::Local,
                enabled: true,
                exclude_if: vec![],
                requires: vec![],
                version_rules: vec![],
                custom_configs: vec![],
                alternatives: vec![],
            }],
        }
        .write_to_file(&modlist_dir.join(RULES_FILENAME))
        .expect("rules file should exist");
        let jar_bytes = b"PK\x03\x04 the user's only copy".to_vec();
        fs::write(local_jars_dir.join("custom-patch.jar"), &jar_bytes)
            .expect("local jar should exist");
        jar_bytes
    }

    fn archive_names(path: &std::path::Path) -> Vec<String> {
        let archive_file = fs::File::open(path).expect("archive should open");
        let archive = zip::ZipArchive::new(archive_file).expect("archive should parse");
        archive.file_names().map(str::to_string).collect()
    }

    /// D107: il jar di un mod locale (o pinnato, D14) non si può riscaricare,
    /// quindi viaggia sempre con la mod list, senza casella.
    #[test]
    fn export_archive_carries_the_local_jars_of_the_modlist() {
        let root_dir = unique_test_root();
        let jar_bytes = write_modlist_with_local_mod(&root_dir);
        let export_path = root_dir.join("pack.zip");

        export_modlist_from_root(
            &root_dir,
            &ExportModlistInput {
                modlist_name: "Pack".into(),
                destination_path: export_path.display().to_string(),
                rules_json: true,
                config_files: false,
                resource_packs: false,
                data_packs: false,
                shaders: false,
                other_files: false,
                selected_content_paths: Vec::new(),
                selected_other_paths: Vec::new(),
            },
        )
        .expect("export should succeed");

        let archive_file = fs::File::open(&export_path).expect("archive should open");
        let mut archive = zip::ZipArchive::new(archive_file).expect("archive should parse");
        let names: Vec<String> = archive.file_names().map(str::to_string).collect();
        let mut exported = Vec::new();
        archive
            .by_name("Pack/local-jars/custom-patch.jar")
            .unwrap_or_else(|_| {
                panic!("the local jar cited by rules.json must travel; the archive held {names:?}")
            })
            .read_to_end(&mut exported)
            .expect("local jar entry should read");
        assert_eq!(exported, jar_bytes);

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    /// 085 scheda 5: dopo D112 `files` contiene nomi, e i byte stanno in
    /// `config-files/<mod_id>/<id>/`. L'export porta quella cartella così
    /// com'è, che è dove l'import la rimette e il lancio la cerca: due config
    /// con lo stesso nome non si scavalcano nello zip (todo.md:1295).
    #[test]
    fn export_archive_carries_the_custom_config_files_from_the_modlist() {
        use crate::rules::{CustomConfig, ModList, ModSource, Rule};

        let root_dir = unique_test_root();
        let modlist_dir = root_dir.join("mod-lists").join("Pack");
        let files_dir = modlist_dir.join("config-files").join("sodium").join("cc-1");
        fs::create_dir_all(&files_dir).expect("config-files dir should exist");
        fs::write(files_dir.join("sodium-options.json"), b"{\"fancy\":true}")
            .expect("config file should exist");
        let shader_dir = modlist_dir.join("config-files").join("sodium").join("cc-2");
        fs::create_dir_all(&shader_dir).expect("config-files dir should exist");
        fs::write(shader_dir.join("shader.txt"), b"shader").expect("config file should exist");
        let config = |id: &str, target_path: &str, file: &str| CustomConfig {
            id: id.into(),
            mc_versions: vec![],
            loader: "any".into(),
            target_path: target_path.into(),
            files: vec![file.into()],
        };
        ModList {
            modlist_name: "Pack".into(),
            author: "Author".into(),
            description: String::new(),
            rules: vec![Rule {
                mod_id: "sodium".into(),
                source: ModSource::Modrinth,
                enabled: true,
                exclude_if: vec![],
                requires: vec![],
                version_rules: vec![],
                custom_configs: vec![
                    config("cc-1", "", "sodium-options.json"),
                    config("cc-2", "shaderpacks", "shader.txt"),
                ],
                alternatives: vec![],
            }],
        }
        .write_to_file(&modlist_dir.join(RULES_FILENAME))
        .expect("rules file should exist");
        let export_path = root_dir.join("pack.zip");

        export_modlist_from_root(
            &root_dir,
            &ExportModlistInput {
                modlist_name: "Pack".into(),
                destination_path: export_path.display().to_string(),
                rules_json: true,
                config_files: true,
                resource_packs: false,
                data_packs: false,
                shaders: false,
                other_files: false,
                selected_content_paths: Vec::new(),
                selected_other_paths: Vec::new(),
            },
        )
        .expect("export should succeed");

        let names = archive_names(&export_path);
        let archive_file = fs::File::open(&export_path).expect("archive should open");
        let mut archive = zip::ZipArchive::new(archive_file).expect("archive should parse");
        for (entry, bytes) in [
            (
                "Pack/config-files/sodium/cc-1/sodium-options.json",
                &b"{\"fancy\":true}"[..],
            ),
            ("Pack/config-files/sodium/cc-2/shader.txt", &b"shader"[..]),
        ] {
            let mut exported = Vec::new();
            archive
                .by_name(entry)
                .unwrap_or_else(|_| panic!("{entry} must travel; the archive held {names:?}"))
                .read_to_end(&mut exported)
                .expect("config entry should read");
            assert_eq!(exported, bytes);
        }

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    /// D107: la cache globale dei mod è la metà riscaricabile, e l'import la
    /// scarta (`modlist_manager.rs`, `cache/`). Non deve finire nello zip.
    #[test]
    fn export_archive_never_carries_the_global_mods_cache() {
        let root_dir = unique_test_root();
        write_modlist_with_local_mod(&root_dir);
        let cached_jar = crate::launcher_paths::LauncherPaths::new(root_dir.clone())
            .mods_cache_dir()
            .join("fabric")
            .join("AbCdEf12")
            .join("sodium.jar");
        fs::create_dir_all(cached_jar.parent().expect("cached jar has a parent"))
            .expect("mods cache dir should exist");
        fs::write(&cached_jar, b"downloadable again").expect("cached jar should exist");
        let export_path = root_dir.join("pack.zip");

        export_modlist_from_root(
            &root_dir,
            &ExportModlistInput {
                modlist_name: "Pack".into(),
                destination_path: export_path.display().to_string(),
                rules_json: true,
                config_files: false,
                resource_packs: false,
                data_packs: false,
                shaders: false,
                other_files: false,
                selected_content_paths: Vec::new(),
                selected_other_paths: Vec::new(),
            },
        )
        .expect("export should succeed");

        let names = archive_names(&export_path);
        assert!(
            !names.iter().any(|name| name.starts_with("Pack/cache/mods")),
            "the global mods cache must not be exported; the archive held {names:?}"
        );

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    /// Una mod list senza mod locali ha `local-jars/` vuota, e una importata
    /// da un `rules.json` nudo non ce l'ha proprio: l'export di entrambe deve
    /// riuscire senza voci per quella cartella.
    #[test]
    fn export_without_local_jars_succeeds_and_adds_no_entry_for_them() {
        let root_dir = unique_test_root();
        let modlist_dir = root_dir.join("mod-lists").join("Pack");
        fs::create_dir_all(modlist_dir.join("local-jars")).expect("local-jars dir should exist");
        fs::write(modlist_dir.join(RULES_FILENAME), b"{}\n").expect("rules file should exist");

        for case in ["empty", "absent"] {
            if case == "absent" {
                fs::remove_dir(modlist_dir.join("local-jars"))
                    .expect("empty local-jars dir should be removable");
            }
            let export_path = root_dir.join(format!("pack-{case}.zip"));
            export_modlist_from_root(
                &root_dir,
                &ExportModlistInput {
                    modlist_name: "Pack".into(),
                    destination_path: export_path.display().to_string(),
                    rules_json: true,
                    config_files: false,
                    resource_packs: false,
                    data_packs: false,
                    shaders: false,
                    other_files: false,
                    selected_content_paths: Vec::new(),
                    selected_other_paths: Vec::new(),
                },
            )
            .unwrap_or_else(|error| {
                panic!("export with {case} local-jars should succeed: {error:#}")
            });

            let names = archive_names(&export_path);
            assert_eq!(
                names,
                vec!["Pack/rules.json".to_string()],
                "{case} local-jars must add no entry"
            );
        }

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }
}
