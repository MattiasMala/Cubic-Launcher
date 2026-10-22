//! The global screenshot view's backend.
//!
//! One flat listing of every
//! `mod-lists/<modlist>/instances/<instance>/screenshots/` directory, plus the
//! first deliberate deletion of a user file this launcher performs.
//!
//! Two decisions worth stating once, because both are load-bearing:
//!
//! - **The date comes from the filesystem, not from the filename.** Minecraft
//!   names screenshots `YYYY-MM-DD_HH.MM.SS.png`, which is convenient and
//!   wrong to trust: a file renamed by hand, or copied over from another
//!   machine, would scatter the ordering exactly where the user trusts it
//!   most. `modified_ms` is `Metadata::modified`.
//! - **A missing `screenshots/` directory is the normal case**, not an error:
//!   an instance that never ran never had one. It is skipped silently — no
//!   log, no banner.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use tauri::State;

use crate::launcher_paths::LauncherPaths;
use crate::path_safety::validate_path_component;

const INSTANCES_DIR_NAME: &str = "instances";
const SCREENSHOTS_DIR_NAME: &str = "screenshots";

/// Extensions the view lists and the deletion accepts. Minecraft only ever
/// writes `.png`; the other two are here because a user drops files into the
/// folder and the view should not pretend they are invisible.
const SCREENSHOT_EXTENSIONS: [&str; 3] = ["png", "jpg", "jpeg"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenshotEntry {
    /// Absolute path: the identity of a row, and what the full-size preview
    /// reads. The delete and the thumbnail take the three names instead.
    pub path: String,
    pub file_name: String,
    pub modlist_name: String,
    pub instance_name: String,
    /// Filesystem mtime in milliseconds since the epoch (negative before it).
    pub modified_ms: i64,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenshotListing {
    pub entries: Vec<ScreenshotEntry>,
    /// Whether this platform has a system trash at all. The confirmation
    /// opens with the "moves to the trash" wording when it is true and with
    /// the permanent one when it is false; a trash that turns out not to work
    /// is caught by the deletion itself, which refuses instead of unlinking.
    pub trash_supported: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ScreenshotDisposal {
    /// Moved into the freedesktop trash; recoverable from the file manager.
    Trashed,
    /// Unlinked. Only reachable when the caller explicitly allowed it.
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteScreenshotOutcome {
    pub disposal: ScreenshotDisposal,
    /// The screenshot that is gone, as the caller named it.
    pub path: String,
}

// ── Listing ──────────────────────────────────────────────────────────────────

pub fn list_screenshots(root_dir: &Path) -> Result<Vec<ScreenshotEntry>> {
    let modlists_dir = LauncherPaths::new(root_dir.to_path_buf())
        .modlists_dir()
        .to_path_buf();

    let mut entries: Vec<ScreenshotEntry> = Vec::new();
    for modlist_dir in child_directories(&modlists_dir)? {
        let Some(modlist_name) = utf8_file_name(&modlist_dir) else {
            continue;
        };
        for instance_dir in child_directories(&modlist_dir.join(INSTANCES_DIR_NAME))? {
            let Some(instance_name) = utf8_file_name(&instance_dir) else {
                continue;
            };
            collect_screenshots(
                &instance_dir.join(SCREENSHOTS_DIR_NAME),
                &modlist_name,
                &instance_name,
                &mut entries,
            )?;
        }
    }

    // Newest first. The path is the tie-break so that two files written in the
    // same millisecond still come out in a stable order.
    entries.sort_by(|left, right| {
        right
            .modified_ms
            .cmp(&left.modified_ms)
            .then_with(|| left.path.cmp(&right.path))
    });

    Ok(entries)
}

/// Directory children of `dir`, sorted. A missing `dir` yields nothing:
/// a mod list with no `instances/` and an instance with no `screenshots/` are
/// both ordinary states.
///
/// Symlinked children are not followed, so a link cannot make the listing
/// report a file that lives outside the launcher root.
fn child_directories(dir: &Path) -> Result<Vec<PathBuf>> {
    let read_dir = match fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", dir.display()))
        }
    };

    let mut directories = Vec::new();
    for entry in read_dir {
        let entry =
            entry.with_context(|| format!("failed to read an entry of {}", dir.display()))?;
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            directories.push(entry.path());
        }
    }
    directories.sort();
    Ok(directories)
}

fn collect_screenshots(
    screenshots_dir: &Path,
    modlist_name: &str,
    instance_name: &str,
    entries: &mut Vec<ScreenshotEntry>,
) -> Result<()> {
    let read_dir = match fs::read_dir(screenshots_dir) {
        Ok(read_dir) => read_dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read {}", screenshots_dir.display()))
        }
    };

    for entry in read_dir {
        let entry = entry
            .with_context(|| format!("failed to read an entry of {}", screenshots_dir.display()))?;
        if !entry
            .file_type()
            .map(|kind| kind.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        let path = entry.path();
        if !has_screenshot_extension(&path) {
            continue;
        }
        let (Some(file_name), Some(path_text)) = (utf8_file_name(&path), path.to_str()) else {
            continue;
        };
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };

        entries.push(ScreenshotEntry {
            path: path_text.to_string(),
            file_name,
            modlist_name: modlist_name.to_string(),
            instance_name: instance_name.to_string(),
            modified_ms: modified_ms(&metadata),
            size_bytes: metadata.len(),
        });
    }

    Ok(())
}

fn has_screenshot_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            SCREENSHOT_EXTENSIONS
                .iter()
                .any(|allowed| extension.eq_ignore_ascii_case(allowed))
        })
        .unwrap_or(false)
}

fn utf8_file_name(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_string())
}

fn modified_ms(metadata: &fs::Metadata) -> i64 {
    let Ok(modified) = metadata.modified() else {
        return 0;
    };
    match modified.duration_since(UNIX_EPOCH) {
        Ok(since_epoch) => i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX),
        Err(before_epoch) => i64::try_from(before_epoch.duration().as_millis())
            .map(|millis| -millis)
            .unwrap_or(i64::MIN),
    }
}

// ── The screenshot a deletion is allowed to touch ────────────────────────────

/// Rebuild one screenshot's path from the three names the listing handed out,
/// or refuse.
///
/// The caller never passes a path. It passes `(modlist, instance, file_name)`,
/// and each one MUST be a single path component — `validate_path_component`,
/// the primitive `launch_preview_content::validate_content_filename` already
/// uses for the same reason (`launch_preview_content.rs:68-70`). A traversal
/// cannot even be spelled this way: `".."` is rejected outright and
/// `"../../options.txt"` carries separators.
///
/// That leaves the filesystem, which the names cannot speak for: the join is
/// canonicalised and compared against `<root>/mod-lists`, so a `screenshots`
/// directory that is a symlink elsewhere resolves to its real location and
/// fails the prefix test.
fn resolve_screenshot(
    root_dir: &Path,
    modlist_name: &str,
    instance_name: &str,
    file_name: &str,
) -> Result<PathBuf> {
    validate_path_component(modlist_name)
        .with_context(|| format!("invalid mod list name '{modlist_name}'"))?;
    validate_path_component(instance_name)
        .with_context(|| format!("invalid instance name '{instance_name}'"))?;
    validate_path_component(file_name)
        .with_context(|| format!("invalid screenshot name '{file_name}'"))?;

    if !has_screenshot_extension(Path::new(file_name)) {
        bail!("'{file_name}' is not a screenshot file");
    }

    let modlists_dir = LauncherPaths::new(root_dir.to_path_buf())
        .modlists_dir()
        .to_path_buf();
    let screenshots_dir = modlists_dir
        .join(modlist_name)
        .join(INSTANCES_DIR_NAME)
        .join(instance_name)
        .join(SCREENSHOTS_DIR_NAME);

    let resolved_dir = fs::canonicalize(&screenshots_dir)
        .with_context(|| format!("no screenshots folder at {}", screenshots_dir.display()))?;
    let modlists_dir = fs::canonicalize(&modlists_dir)
        .with_context(|| format!("failed to resolve {}", modlists_dir.display()))?;

    let relative = resolved_dir.strip_prefix(&modlists_dir).map_err(|_| {
        anyhow::anyhow!(
            "the screenshots folder of '{modlist_name}/{instance_name}' is outside the \
             mod lists directory"
        )
    })?;
    let segments: Vec<&std::ffi::OsStr> = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(segment) => Some(segment),
            _ => None,
        })
        .collect();
    let shaped_like_a_screenshots_folder = segments.len() == 4
        && segments[1] == INSTANCES_DIR_NAME
        && segments[3] == SCREENSHOTS_DIR_NAME;
    if !shaped_like_a_screenshots_folder {
        bail!(
            "the screenshots folder of '{modlist_name}/{instance_name}' is outside the \
             mod lists directory"
        );
    }

    let file_path = resolved_dir.join(file_name);
    // `symlink_metadata` does not follow: a symlink is refused here rather
    // than resolved, so the delete only ever touches a real file sitting in
    // the folder it claims to sit in.
    let metadata = fs::symlink_metadata(&file_path)
        .with_context(|| format!("no screenshot at {}", file_path.display()))?;
    if !metadata.is_file() {
        bail!("{} is not a regular file", file_path.display());
    }

    Ok(file_path)
}

// ── Deleting ─────────────────────────────────────────────────────────────────

/// Prefix of the error a refused deletion carries when the trash is the thing
/// that failed. The frontend matches on it to re-ask with the permanent
/// wording instead of showing a bare failure.
pub const TRASH_UNAVAILABLE: &str = "trash-unavailable:";

/// The injected "put this in the trash" step of `delete_screenshot_with`; `None`
/// is a platform with no trash. Same lifetimes `Option<&dyn Fn(..)>` elides to.
type SendToTrash<'a> = Option<&'a (dyn Fn(&Path) -> Result<()> + 'a)>;

pub fn delete_screenshot(
    root_dir: &Path,
    modlist_name: &str,
    instance_name: &str,
    file_name: &str,
    allow_permanent: bool,
) -> Result<DeleteScreenshotOutcome> {
    let send_to_trash = |path: &Path| trash::delete(path).map_err(anyhow::Error::from);
    let send_to_trash: SendToTrash<'_> = if trash_supported() {
        Some(&send_to_trash)
    } else {
        None
    };
    delete_screenshot_with(
        root_dir,
        modlist_name,
        instance_name,
        file_name,
        allow_permanent,
        send_to_trash,
    )
}

/// The deletion with the "put this in the trash" step injected; `None` is a
/// platform with no trash at all. The seam exists so the tests can exercise
/// the contract — moved, refused, or deleted on an explicit confirmation —
/// without ever touching the trash of whoever runs them.
fn delete_screenshot_with(
    root_dir: &Path,
    modlist_name: &str,
    instance_name: &str,
    file_name: &str,
    allow_permanent: bool,
    send_to_trash: SendToTrash<'_>,
) -> Result<DeleteScreenshotOutcome> {
    let resolved = resolve_screenshot(root_dir, modlist_name, instance_name, file_name)?;
    let resolved_text = resolved.display().to_string();

    let trash_error = match send_to_trash {
        Some(send_to_trash) => match send_to_trash(&resolved) {
            Ok(()) => {
                return Ok(DeleteScreenshotOutcome {
                    disposal: ScreenshotDisposal::Trashed,
                    path: resolved_text,
                })
            }
            Err(error) => format!("{error:#}"),
        },
        None => "this platform has no system trash".to_string(),
    };

    // The pact: `allow_permanent` is what the dialog promised. A trash that
    // did not work does not quietly become an unlink — the caller is told,
    // and has to come back having said "permanently".
    if !allow_permanent {
        bail!("{TRASH_UNAVAILABLE} {trash_error}");
    }

    fs::remove_file(&resolved)
        .with_context(|| format!("failed to delete {}", resolved.display()))?;
    Ok(DeleteScreenshotOutcome {
        disposal: ScreenshotDisposal::Deleted,
        path: resolved_text,
    })
}

/// Whether this build has a system trash to aim at **at all**.
///
/// Deliberately not "will the next deletion reach the trash": with the crate
/// doing the work that question has no honest answer short of trying, and
/// probing by trashing a throwaway file would leave litter in the user's bin.
/// So this answers the platform question — the one that can be answered
/// without guessing — and the runtime answer comes from the attempt itself,
/// which refuses rather than deletes when it fails (`TRASH_UNAVAILABLE`).
pub fn trash_supported() -> bool {
    cfg!(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "windows",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd",
    ))
}

// ── Thumbnails ───────────────────────────────────────────────────────────────

/// Where the downscaled copies live: under the launcher's `cache/`, never next
/// to the screenshots — that folder is the user's, and a file manager opened on
/// it should show what the game wrote and nothing else.
const THUMBNAILS_DIR: &str = "screenshot-thumbnails";

/// Bumped when the way a thumbnail is made changes, so old copies stop
/// matching instead of being served.
const THUMBNAIL_FORMAT: u32 = 1;

/// The widest copy anyone asks for. The grid asks for far less; the large
/// preview of a file too big to send whole (condivisi-8) asks for this.
const MAX_THUMBNAIL_WIDTH: u32 = 2560;

/// A screenshot whose file is larger than this is never sent whole over the
/// IPC: the same ceiling `read_image_as_data_url` applies.
const MAX_WHOLE_BYTES: u64 = 16 * 1024 * 1024;

/// What the PNG decoder may allocate. Only an interlaced PNG is decoded whole
/// — Minecraft never writes one — so this is the ceiling of a rare case, not
/// of every thumbnail.
const DECODER_LIMIT_BYTES: usize = 512 * 1024 * 1024;

/// How many thumbnails are made at once. The grid asks for every one that
/// scrolls into view; past two at a time the decodes only fight over the CPU
/// and the memory.
static THUMBNAIL_PERMITS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// A downscaled copy of one screenshot as a `data:` URL, made once and then
/// read from the cache (condivisi-7).
///
/// The screenshot is named by the same three names the deletion takes and
/// goes through the same containment check. A PNG is decoded row by row and
/// averaged down to `max_width` (never upscaled), so a 2560×1440 file costs a
/// few hundred kilobytes of memory and of IPC instead of 15 MB of pixels and
/// 4 MB of base64. A JPEG — never written by the game, only dropped into the
/// folder by hand — has no decoder here and is sent whole when it is small
/// enough.
pub fn screenshot_thumbnail(
    root_dir: &Path,
    modlist_name: &str,
    instance_name: &str,
    file_name: &str,
    max_width: u32,
) -> Result<String> {
    let resolved = resolve_screenshot(root_dir, modlist_name, instance_name, file_name)?;
    let metadata = fs::metadata(&resolved)
        .with_context(|| format!("failed to read {}", resolved.display()))?;
    let max_width = max_width.clamp(1, MAX_THUMBNAIL_WIDTH);

    let is_png = resolved
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("png"));
    if !is_png {
        if metadata.len() > MAX_WHOLE_BYTES {
            bail!(
                "{} is a {} MB JPEG: too large to send whole, and only PNG screenshots get a \
                 thumbnail",
                resolved.display(),
                metadata.len() / 1_000_000
            );
        }
        let bytes = fs::read(&resolved)
            .with_context(|| format!("failed to read {}", resolved.display()))?;
        return Ok(data_url("image/jpeg", &bytes));
    }

    let cached = thumbnail_cache_path(root_dir, &resolved, &metadata, max_width);
    if let Ok(bytes) = fs::read(&cached) {
        return Ok(data_url("image/png", &bytes));
    }

    let file = fs::File::open(&resolved)
        .with_context(|| format!("failed to open {}", resolved.display()))?;
    let thumbnail = downscale_png(std::io::BufReader::new(file), max_width)
        .with_context(|| format!("failed to make a thumbnail of {}", resolved.display()))?;

    // A cache that cannot be written costs the next view another decode, not
    // the thumbnail: the bytes are returned either way.
    if let Some(parent) = cached.parent() {
        if fs::create_dir_all(parent).is_ok() {
            let _ = crate::atomic_write::write_atomically(&cached, &thumbnail);
        }
    }
    Ok(data_url("image/png", &thumbnail))
}

fn data_url(mime: &str, bytes: &[u8]) -> String {
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
    format!("data:{mime};base64,{}", BASE64.encode(bytes))
}

/// The cached copy's name says which file, which version of it and which size:
/// a screenshot rewritten in place gets a new mtime and so a new thumbnail.
fn thumbnail_cache_path(
    root_dir: &Path,
    resolved: &Path,
    metadata: &fs::Metadata,
    max_width: u32,
) -> PathBuf {
    use sha2::{Digest, Sha256};

    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_nanos())
        .unwrap_or(0);
    let key = format!(
        "{THUMBNAIL_FORMAT}|{}|{}|{modified_ns}|{max_width}",
        resolved.display(),
        metadata.len()
    );
    root_dir
        .join("cache")
        .join(THUMBNAILS_DIR)
        .join(format!("{:x}.png", Sha256::digest(key.as_bytes())))
}

/// Average the source pixels into a picture at most `max_width` wide, keeping
/// the aspect ratio, and encode it as an RGB PNG.
fn downscale_png(reader: impl std::io::Read, max_width: u32) -> Result<Vec<u8>> {
    let mut decoder = png::Decoder::new_with_limits(
        reader,
        png::Limits {
            bytes: DECODER_LIMIT_BYTES,
        },
    );
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().context("not a readable PNG")?;

    let (width, height) = reader.info().size();
    if width == 0 || height == 0 {
        bail!("the PNG has no pixels");
    }
    let (color, _) = reader.output_color_type();
    let channels = match color {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => bail!("the PNG palette was not expanded"),
    };

    let out_width = width.min(max_width);
    let out_height = ((u64::from(height) * u64::from(out_width) + u64::from(width) / 2)
        / u64::from(width))
    .max(1) as u32;
    let mut scaler = BoxScaler::new(width, height, out_width, out_height, channels);

    if reader.info().interlaced {
        let mut frame = vec![0; reader.output_buffer_size()];
        let info = reader
            .next_frame(&mut frame)
            .context("not a readable PNG")?;
        for row in frame.chunks(info.line_size).take(height as usize) {
            scaler.push_row(row);
        }
    } else {
        while let Some(row) = reader.next_row().context("not a readable PNG")? {
            scaler.push_row(row.data());
        }
    }
    let pixels = scaler.finish();

    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, out_width, out_height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .context("failed to encode the thumbnail")?;
        writer
            .write_image_data(&pixels)
            .context("failed to encode the thumbnail")?;
        writer.finish().context("failed to encode the thumbnail")?;
    }
    Ok(encoded)
}

/// A box filter fed one source row at a time, so the memory it needs is two
/// rows of sums rather than the whole picture.
struct BoxScaler {
    channels: usize,
    out_width: usize,
    out_height: u32,
    height: u32,
    /// For each source column, the output column it falls into.
    column_of: Vec<usize>,
    sums: Vec<u64>,
    counts: Vec<u64>,
    current_out_row: u32,
    source_row: u32,
    pixels: Vec<u8>,
}

impl BoxScaler {
    fn new(width: u32, height: u32, out_width: u32, out_height: u32, channels: usize) -> Self {
        let column_of = (0..u64::from(width))
            .map(|x| (x * u64::from(out_width) / u64::from(width)) as usize)
            .collect();
        Self {
            channels,
            out_width: out_width as usize,
            out_height,
            height,
            column_of,
            sums: vec![0; out_width as usize * 3],
            counts: vec![0; out_width as usize],
            current_out_row: 0,
            source_row: 0,
            pixels: Vec::with_capacity(out_width as usize * out_height as usize * 3),
        }
    }

    fn push_row(&mut self, row: &[u8]) {
        if self.source_row >= self.height {
            return;
        }
        let out_row = (u64::from(self.source_row) * u64::from(self.out_height)
            / u64::from(self.height)) as u32;
        while self.current_out_row < out_row {
            self.flush_row();
        }

        for (x, pixel) in row.chunks_exact(self.channels).enumerate() {
            let Some(&column) = self.column_of.get(x) else {
                break;
            };
            let rgb = match self.channels {
                1 | 2 => [pixel[0]; 3],
                _ => [pixel[0], pixel[1], pixel[2]],
            };
            for (channel, value) in rgb.into_iter().enumerate() {
                self.sums[column * 3 + channel] += u64::from(value);
            }
            self.counts[column] += 1;
        }
        self.source_row += 1;
    }

    fn flush_row(&mut self) {
        for column in 0..self.out_width {
            let count = self.counts[column].max(1);
            for channel in 0..3 {
                let sum = self.sums[column * 3 + channel];
                self.pixels.push(((sum + count / 2) / count) as u8);
            }
        }
        self.sums.fill(0);
        self.counts.fill(0);
        self.current_out_row += 1;
    }

    fn finish(mut self) -> Vec<u8> {
        while self.current_out_row < self.out_height {
            self.flush_row();
        }
        self.pixels
    }
}

// ── Commands ─────────────────────────────────────────────────────────────────

#[tauri::command]
pub fn list_screenshots_command(
    launcher_paths: State<'_, LauncherPaths>,
) -> Result<ScreenshotListing, String> {
    let root_dir = launcher_paths.root_dir();
    let entries = list_screenshots(root_dir).map_err(|error| error.to_string())?;
    Ok(ScreenshotListing {
        entries,
        trash_supported: trash_supported(),
    })
}

/// A thumbnail of one screenshot, off the main thread and a couple at a time
/// (condivisi-7). The grid asks for one when it scrolls into view; the large
/// preview asks for a wide one when the file is too big to send whole
/// (condivisi-8).
#[tauri::command]
pub async fn screenshot_thumbnail_command(
    launcher_paths: State<'_, LauncherPaths>,
    modlist_name: String,
    instance_name: String,
    file_name: String,
    max_width: u32,
) -> Result<String, String> {
    let root_dir = launcher_paths.root_dir().to_path_buf();
    let _permit = THUMBNAIL_PERMITS
        .acquire()
        .await
        .map_err(|error| error.to_string())?;
    tauri::async_runtime::spawn_blocking(move || {
        screenshot_thumbnail(
            &root_dir,
            &modlist_name,
            &instance_name,
            &file_name,
            max_width,
        )
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| format!("{error:#}"))
}

/// `allow_permanent` is the confirmation the user actually saw: the dialog
/// only sets it when it said the deletion is permanent. If the trash turns
/// out to be unusable and it is false, the deletion is refused rather than
/// silently promoted to a permanent one.
#[tauri::command]
pub fn delete_screenshot_command(
    launcher_paths: State<'_, LauncherPaths>,
    modlist_name: String,
    instance_name: String,
    file_name: String,
    allow_permanent: bool,
) -> Result<DeleteScreenshotOutcome, String> {
    delete_screenshot(
        launcher_paths.root_dir(),
        &modlist_name,
        &instance_name,
        &file_name,
        allow_permanent,
    )
    .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
pub fn open_screenshot_folder_command(
    launcher_paths: State<'_, LauncherPaths>,
    modlist_name: String,
    instance_name: String,
    file_name: String,
) -> Result<(), String> {
    let folder = screenshot_folder(
        launcher_paths.root_dir(),
        &modlist_name,
        &instance_name,
        &file_name,
    )
    .map_err(|error| format!("{error:#}"))?;
    open::that(&folder).map_err(|error| format!("failed to open {}: {error}", folder.display()))
}

/// The folder the file manager opens for one screenshot.
///
/// The containment check runs on the canonical path, inside
/// `resolve_screenshot`; the file manager gets the same path without the
/// `\\?\` prefix Windows' `canonicalize` adds, which `cmd /c start` rejects
/// (windows-3). `dunce::simplified` keeps the prefix only where the path
/// cannot be spelled without it.
fn screenshot_folder(
    root_dir: &Path,
    modlist_name: &str,
    instance_name: &str,
    file_name: &str,
) -> Result<PathBuf> {
    let resolved = resolve_screenshot(root_dir, modlist_name, instance_name, file_name)?;
    let folder = resolved
        .parent()
        .with_context(|| format!("'{file_name}' has no folder"))?;
    Ok(dunce::simplified(folder).to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::time::{Duration, SystemTime};

    use super::*;

    fn unique_root(tag: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        env::temp_dir().join(format!("cubic-screenshots-{tag}-{stamp}"))
    }

    fn screenshots_dir(root: &Path, modlist: &str, instance: &str) -> PathBuf {
        root.join("mod-lists")
            .join(modlist)
            .join(INSTANCES_DIR_NAME)
            .join(instance)
            .join(SCREENSHOTS_DIR_NAME)
    }

    /// Write a screenshot with an exact mtime, so the ordering test measures
    /// the filesystem date and not the order of creation.
    fn write_screenshot(
        root: &Path,
        modlist: &str,
        instance: &str,
        name: &str,
        mtime_secs: u64,
    ) -> PathBuf {
        let directory = screenshots_dir(root, modlist, instance);
        fs::create_dir_all(&directory).expect("failed to create the screenshots directory");
        let path = directory.join(name);
        fs::write(&path, b"not really a png").expect("failed to write the screenshot");
        let handle = fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("failed to reopen the screenshot");
        handle
            .set_times(
                fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(mtime_secs)),
            )
            .expect("failed to set the screenshot mtime");
        path
    }

    #[test]
    fn the_folder_for_the_file_manager_is_the_plain_form_of_the_screenshots_folder() {
        let root = unique_root("open-folder");
        write_screenshot(&root, "Drehmal", "1.20.1-forge", "a.png", 1_000);

        let folder = screenshot_folder(&root, "Drehmal", "1.20.1-forge", "a.png")
            .expect("the folder should resolve");

        assert!(
            !folder.to_string_lossy().starts_with(r"\\?\"),
            "cmd /c start does not accept a verbatim path: {}",
            folder.display()
        );
        assert_eq!(
            fs::canonicalize(&folder).unwrap(),
            fs::canonicalize(screenshots_dir(&root, "Drehmal", "1.20.1-forge")).unwrap()
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A real PNG, `width`×`height`, left half black and right half white.
    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut pixels = Vec::new();
        for _ in 0..height {
            for x in 0..width {
                let value = if x < width / 2 { 0 } else { 255 };
                pixels.extend_from_slice(&[value, value, value, 255]);
            }
        }
        let mut encoded = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut encoded, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&pixels).unwrap();
        }
        encoded
    }

    fn decode(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let mut reader = png::Decoder::new(bytes).read_info().unwrap();
        let mut frame = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut frame).unwrap();
        frame.truncate(info.buffer_size());
        (info.width, info.height, frame)
    }

    fn decode_data_url(url: &str) -> Vec<u8> {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
        BASE64
            .decode(url.strip_prefix("data:image/png;base64,").unwrap())
            .unwrap()
    }

    #[test]
    fn a_thumbnail_is_a_smaller_png_with_the_same_picture() {
        let thumbnail = downscale_png(png_bytes(64, 36).as_slice(), 16).unwrap();
        let (width, height, pixels) = decode(&thumbnail);

        assert_eq!((width, height), (16, 9));
        // Left column black, right column white: the averaging did not smear
        // the two halves into grey.
        assert_eq!(&pixels[0..3], &[0, 0, 0]);
        assert_eq!(&pixels[(15 * 3)..(16 * 3)], &[255, 255, 255]);
    }

    #[test]
    fn a_thumbnail_is_never_larger_than_the_screenshot() {
        let thumbnail = downscale_png(png_bytes(8, 4).as_slice(), 480).unwrap();
        assert_eq!(decode(&thumbnail).0, 8);
    }

    #[test]
    fn a_thumbnail_is_made_once_and_then_read_from_the_cache() {
        let root = unique_root("thumbnail-cache");
        let path = write_screenshot(&root, "Drehmal", "1.20.1-forge", "a.png", 1_000);
        fs::write(&path, png_bytes(40, 20)).unwrap();
        let handle = fs::OpenOptions::new().write(true).open(&path).unwrap();
        handle
            .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1_000)))
            .unwrap();

        let first = screenshot_thumbnail(&root, "Drehmal", "1.20.1-forge", "a.png", 10).unwrap();
        assert_eq!(decode(&decode_data_url(&first)).0, 10);

        let cache_dir = root.join("cache").join(THUMBNAILS_DIR);
        let cached: Vec<_> = fs::read_dir(&cache_dir).unwrap().flatten().collect();
        assert_eq!(cached.len(), 1, "one thumbnail on disk");
        assert!(
            !screenshots_dir(&root, "Drehmal", "1.20.1-forge")
                .read_dir()
                .unwrap()
                .flatten()
                .any(|entry| entry.file_name() != "a.png"),
            "nothing is written next to the user's screenshots"
        );

        // The cached copy is what comes back the second time.
        fs::write(cached[0].path(), png_bytes(3, 3)).unwrap();
        let second = screenshot_thumbnail(&root, "Drehmal", "1.20.1-forge", "a.png", 10).unwrap();
        assert_eq!(decode(&decode_data_url(&second)).0, 3);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_screenshot_too_large_to_send_whole_still_gets_a_thumbnail() {
        // condivisi-8: past 16 MB the full read refuses, and the preview stayed
        // on «Loading…». The thumbnail has no such ceiling. Uncompressed rows
        // make a real PNG of that size without a large picture.
        let root = unique_root("thumbnail-large");
        let path = write_screenshot(&root, "Drehmal", "1.20.1-forge", "big.png", 1_000);
        let (width, height) = (2048u32, 2100u32);
        let mut encoded = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut encoded, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_compression(png::Compression::Fast);
            encoder.set_filter(png::FilterType::NoFilter);
            let mut writer = encoder.write_header().unwrap();
            let mut seed = 0x2545_f491_u32;
            let pixels: Vec<u8> = (0..(width * height * 4))
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    seed as u8
                })
                .collect();
            writer.write_image_data(&pixels).unwrap();
        }
        fs::write(&path, &encoded).unwrap();
        assert!(fs::metadata(&path).unwrap().len() > MAX_WHOLE_BYTES);

        let preview =
            screenshot_thumbnail(&root, "Drehmal", "1.20.1-forge", "big.png", 512).unwrap();
        assert_eq!(decode(&decode_data_url(&preview)).0, 512);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_thumbnail_is_refused_outside_the_screenshots_folders() {
        let root = unique_root("thumbnail-escape");
        write_screenshot(&root, "Drehmal", "1.20.1-forge", "a.png", 1_000);

        assert!(screenshot_thumbnail(&root, "..", "1.20.1-forge", "a.png", 10).is_err());
        assert!(screenshot_thumbnail(&root, "Drehmal", "1.20.1-forge", "../../a.png", 10).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn lists_every_instance_and_ignores_instances_without_a_screenshots_folder() {
        let root = unique_root("listing");
        write_screenshot(
            &root,
            "Drehmal",
            "1.20.1-forge",
            "2026-08-18_22.08.58.png",
            1_000,
        );
        write_screenshot(
            &root,
            "Drehmal",
            "1.20.1-forge",
            "2026-09-19_22.47.09.png",
            3_000,
        );
        write_screenshot(
            &root,
            "test2",
            "26.3-fabric",
            "2026-09-19_22.50.44.png",
            4_000,
        );
        // The 1.20.1-fabric case on the real disk: an instance directory that
        // exists and has no screenshots folder at all.
        fs::create_dir_all(
            root.join("mod-lists")
                .join("Drehmal")
                .join(INSTANCES_DIR_NAME)
                .join("1.20.1-fabric")
                .join("mods"),
        )
        .expect("failed to create the instance without screenshots");

        let entries = list_screenshots(&root).expect("listing must not fail");

        let described: Vec<(String, String, String)> = entries
            .iter()
            .map(|entry| {
                (
                    entry.modlist_name.clone(),
                    entry.instance_name.clone(),
                    entry.file_name.clone(),
                )
            })
            .collect();
        assert_eq!(
            described,
            vec![
                (
                    "test2".into(),
                    "26.3-fabric".into(),
                    "2026-09-19_22.50.44.png".into()
                ),
                (
                    "Drehmal".into(),
                    "1.20.1-forge".into(),
                    "2026-09-19_22.47.09.png".into()
                ),
                (
                    "Drehmal".into(),
                    "1.20.1-forge".into(),
                    "2026-08-18_22.08.58.png".into()
                ),
            ]
        );
        assert_eq!(entries[0].modified_ms, 4_000_000);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn orders_by_filesystem_time_not_by_filename() {
        let root = unique_root("ordering");
        // The filename says this one is the oldest by two years; the mtime
        // says it is the newest. A file renamed by hand must not jump the
        // queue.
        write_screenshot(&root, "pack", "instance", "2024-01-01_00.00.00.png", 9_000);
        write_screenshot(&root, "pack", "instance", "2026-09-19_22.47.09.png", 5_000);

        let entries = list_screenshots(&root).expect("listing must not fail");
        let names: Vec<&str> = entries
            .iter()
            .map(|entry| entry.file_name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["2024-01-01_00.00.00.png", "2026-09-19_22.47.09.png"]
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_launcher_root_without_mod_lists_lists_nothing_and_is_not_an_error() {
        let root = unique_root("empty");
        fs::create_dir_all(&root).expect("failed to create the root");

        let entries = list_screenshots(&root).expect("a missing mod-lists directory is normal");
        assert!(entries.is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn refuses_every_name_that_leaves_the_screenshots_folder() {
        let root = unique_root("refusal");
        let screenshot = write_screenshot(&root, "pack", "instance", "shot.png", 1_000);
        let instance_dir = screenshot
            .parent()
            .and_then(|parent| parent.parent())
            .expect("the instance directory must exist")
            .to_path_buf();

        let options_txt = instance_dir.join("options.txt.png");
        fs::write(&options_txt, b"a file that is not in screenshots/")
            .expect("failed to write the decoy");
        let subfolder = screenshot
            .parent()
            .expect("parent")
            .join("nested")
            .join("shot.png");
        fs::create_dir_all(subfolder.parent().expect("parent")).expect("failed to nest");
        fs::write(&subfolder, b"one level too deep").expect("failed to write the nested decoy");

        // Every shape that tries to name something other than one file inside
        // one known instance's screenshots folder, with the reason it must be
        // refused for: a refusal that happens for the wrong reason is a test
        // that stops protecting anything the day the reason changes.
        for (modlist, instance, file_name, reason) in [
            // Climbing out of the folder, the directory, and the root.
            (
                "pack",
                "instance",
                "../options.txt.png",
                "must not contain a path separator",
            ),
            (
                "pack",
                "instance",
                "../../../../../../etc/passwd.png",
                "must not contain a path separator",
            ),
            ("pack", "instance", "..", "is not allowed"),
            ("pack", "..", "shot.png", "invalid instance name"),
            ("..", "instance", "shot.png", "invalid mod list name"),
            // An absolute path is not a component either.
            (
                "pack",
                "instance",
                "/etc/passwd.png",
                "must not contain a path separator",
            ),
            // A subfolder of screenshots/ is one level too deep.
            (
                "pack",
                "instance",
                "nested/shot.png",
                "must not contain a path separator",
            ),
            // Empty names, and a file that is not an image.
            ("pack", "instance", "", "cannot be empty"),
            ("", "instance", "shot.png", "cannot be empty"),
            ("pack", "instance", "shot.txt", "is not a screenshot file"),
            // A folder that does not exist at all.
            ("pack", "ghost", "shot.png", "no screenshots folder at"),
        ] {
            let error = delete_screenshot_with(&root, modlist, instance, file_name, true, None)
                .expect_err(&format!(
                    "'{modlist}/{instance}/{file_name}' must be refused"
                ));
            let reported = format!("{error:#}");
            assert!(
                reported.contains(reason),
                "'{modlist}/{instance}/{file_name}' was refused for the wrong reason: {reported}"
            );
            assert!(
                screenshot.exists() && options_txt.exists(),
                "the refusal of '{modlist}/{instance}/{file_name}' deleted something"
            );
        }

        // Nothing was deleted by any of the refusals.
        assert!(screenshot.exists());
        assert!(options_txt.exists());
        assert!(subfolder.exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_screenshots_folder_that_is_a_symlink_out_of_the_root() {
        let root = unique_root("symlink");
        let elsewhere = unique_root("symlink-target");
        fs::create_dir_all(&elsewhere).expect("failed to create the link target");
        let victim = elsewhere.join("secret.png");
        fs::write(&victim, b"not the launcher's file").expect("failed to write the victim");

        let instance_dir = root
            .join("mod-lists")
            .join("pack")
            .join(INSTANCES_DIR_NAME)
            .join("instance");
        fs::create_dir_all(&instance_dir).expect("failed to create the instance");
        std::os::unix::fs::symlink(&elsewhere, instance_dir.join(SCREENSHOTS_DIR_NAME))
            .expect("failed to link");

        let error = delete_screenshot_with(&root, "pack", "instance", "secret.png", true, None)
            .expect_err("a linked screenshots folder must be refused");
        assert!(
            error
                .to_string()
                .contains("outside the mod lists directory"),
            "unexpected refusal: {error}"
        );
        assert!(victim.exists(), "the linked-to file must survive");

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&elsewhere);
    }

    /// A stand-in for `trash::delete`: moves the file into `bin` instead of
    /// into whoever runs the tests' real trash.
    fn bin_into(bin: &Path) -> impl Fn(&Path) -> Result<()> + '_ {
        move |path: &Path| {
            fs::create_dir_all(bin)?;
            let name = path.file_name().context("no file name")?;
            fs::rename(path, bin.join(name))?;
            Ok(())
        }
    }

    #[test]
    fn a_trashed_screenshot_leaves_its_folder_and_says_so() {
        let root = unique_root("trashed");
        let bin = unique_root("trashed-bin");
        let screenshot = write_screenshot(&root, "pack", "instance", "shot.png", 1_000);
        let send_to_trash = bin_into(&bin);

        let outcome = delete_screenshot_with(
            &root,
            "pack",
            "instance",
            "shot.png",
            false,
            Some(&send_to_trash),
        )
        .expect("the deletion must succeed");

        assert_eq!(outcome.disposal, ScreenshotDisposal::Trashed);
        assert!(!screenshot.exists(), "the screenshot must leave its folder");
        assert!(
            bin.join("shot.png").exists(),
            "the trash must have received it"
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&bin);
    }

    #[test]
    fn a_trash_that_fails_refuses_instead_of_deleting() {
        let root = unique_root("trash-fails");
        let screenshot = write_screenshot(&root, "pack", "instance", "shot.png", 1_000);
        let broken_trash = |_: &Path| bail!("no trash on this machine");

        let error = delete_screenshot_with(
            &root,
            "pack",
            "instance",
            "shot.png",
            false,
            Some(&broken_trash),
        )
        .expect_err("a failed trashing must not become a deletion");
        let reported = format!("{error:#}");
        assert!(
            reported.contains(TRASH_UNAVAILABLE),
            "unexpected refusal: {reported}"
        );
        assert!(
            reported.contains("no trash on this machine"),
            "the refusal must carry why the trash failed: {reported}"
        );
        assert!(screenshot.exists(), "the refused deletion must not delete");

        // Only the explicit confirmation turns it into an unlink.
        let outcome = delete_screenshot_with(
            &root,
            "pack",
            "instance",
            "shot.png",
            true,
            Some(&broken_trash),
        )
        .expect("a confirmed permanent deletion must go through");
        assert_eq!(outcome.disposal, ScreenshotDisposal::Deleted);
        assert!(!screenshot.exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn without_a_trash_the_deletion_needs_the_permanent_confirmation() {
        let root = unique_root("permanent");
        let screenshot = write_screenshot(&root, "pack", "instance", "shot.png", 1_000);

        let error = delete_screenshot_with(&root, "pack", "instance", "shot.png", false, None)
            .expect_err("a deletion with no trash and no confirmation must be refused");
        let reported = format!("{error:#}");
        assert!(
            reported.contains(TRASH_UNAVAILABLE),
            "unexpected refusal: {reported}"
        );
        assert!(
            reported.contains("no system trash"),
            "unexpected refusal: {reported}"
        );
        assert!(screenshot.exists(), "the refused deletion must not delete");

        let outcome = delete_screenshot_with(&root, "pack", "instance", "shot.png", true, None)
            .expect("a confirmed permanent deletion must go through");
        assert_eq!(outcome.disposal, ScreenshotDisposal::Deleted);
        assert!(!screenshot.exists());

        let _ = fs::remove_dir_all(&root);
    }
}
