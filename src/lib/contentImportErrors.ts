/**
 * The frontend side of `import_local_content_pack_command`.
 *
 * The command does not fail with a sentence: it returns
 * `{ code, message }` (`local_content_packs.rs:526-562`), and the code is what
 * the UI branches on. Matching on the message would break the moment the
 * wording changes, and one generic "import failed" would hide the only two
 * outcomes the user can act on — a taken name and an unsupported source.
 */

/** Mirrors `ContentImportErrorCode` (`local_content_packs.rs:528`). */
export type ContentImportErrorCode =
  | "invalidModlistName"
  | "unknownModlist"
  | "invalidContentType"
  | "invalidSourcePath"
  | "unsupportedSourceType"
  | "nameCollision"
  | "readFailed"
  | "writeFailed";

/** Mirrors `ContentImportError` (`local_content_packs.rs:550`). */
export type ContentImportError = {
  code: ContentImportErrorCode;
  message: string;
};

/** What the upload panel shows when an import is refused. */
export type LocalImportFailure = {
  title: string;
  message: string;
  /** The backend sentence, kept as secondary text instead of being dropped. */
  detail: string;
};

/** Every code the command can return, as a membership table. */
const CODES: Record<ContentImportErrorCode, true> = {
  invalidModlistName: true,
  unknownModlist: true,
  invalidContentType: true,
  invalidSourcePath: true,
  unsupportedSourceType: true,
  nameCollision: true,
  readFailed: true,
  writeFailed: true,
};

/**
 * A Tauri command that fails with a serializable error rejects with the
 * serialized value, so this is an object and not a string — but a panic or a
 * deserialization failure still arrives as a string, hence the guard.
 */
export function asContentImportError(error: unknown): ContentImportError | null {
  if (typeof error !== "object" || error === null) return null;
  const candidate = error as { code?: unknown; message?: unknown };
  if (typeof candidate.code !== "string" || !(candidate.code in CODES)) return null;
  return {
    code: candidate.code as ContentImportErrorCode,
    message: typeof candidate.message === "string" ? candidate.message : "",
  };
}

/**
 * The mod-list folder each category copies into. Mirrors
 * `modlist_category_dir` (`local_content_packs.rs:51`); it is part of the
 * collision message because "delete the leftover file" is not actionable
 * without the folder it is in.
 */
const CATEGORY_DIR: Record<string, string> = {
  resourcepack: "resourcepacks",
  datapack: "datapacks",
  shader: "shaders",
};

/** What to call each category in a sentence the user reads. */
export const CONTENT_TYPE_NOUN: Record<string, string> = {
  resourcepack: "resource pack",
  datapack: "data pack",
  shader: "shader",
};

/** The last path component of a picked or dropped path, `/` or `\`. */
function baseName(path: string): string {
  const cleaned = path.replace(/[/\\]+$/, "");
  const cut = Math.max(cleaned.lastIndexOf("/"), cleaned.lastIndexOf("\\"));
  return cut < 0 ? cleaned : cleaned.slice(cut + 1);
}

export type ImportContext = {
  modlistName: string;
  contentType: string;
  sourcePath: string;
};

/**
 * One message per code, in the words of the thing the user just did.
 *
 * `nameCollision` is the case that looks like a bug and is not (D41, D42):
 * removing an entry from the list leaves its files in the mod list, so a name
 * can be taken by a pack the list no longer shows. The message has to say
 * that, otherwise the only reading left is "the launcher is lying to me".
 */
export function describeContentImportError(error: unknown, context: ImportContext): LocalImportFailure {
  const noun = CONTENT_TYPE_NOUN[context.contentType] ?? "pack";
  const name = baseName(context.sourcePath);
  const parsed = asContentImportError(error);

  if (!parsed) {
    return {
      title: `Could not import '${name}'`,
      message: `The import failed before it could report a reason.`,
      detail: String(error),
    };
  }

  const detail = parsed.message;

  switch (parsed.code) {
    case "nameCollision": {
      const folder = CATEGORY_DIR[context.contentType] ?? context.contentType;
      return {
        title: `'${name}' is already in this mod list`,
        message:
          `The ${folder} folder of '${context.modlistName}' already holds a file called '${name}'. ` +
          `Removing an entry from the list does not delete its files, so a ${noun} imported earlier ` +
          `still holds the name even when the list no longer shows it. Rename this ${noun}, or delete ` +
          `'${name}' from the mod list's ${folder} folder, then import again.`,
        detail,
      };
    }
    case "unsupportedSourceType":
      return {
        title: `'${name}' is not a ${noun}`,
        message: `A ${noun} is either a .zip archive or the folder a .zip was unpacked into. Pick one of those.`,
        detail,
      };
    case "invalidSourcePath":
      return {
        title: `'${name}' could not be used`,
        message: `That path no longer exists, or it has no usable file name. Pick the ${noun} again.`,
        detail,
      };
    case "readFailed":
      return {
        title: `'${name}' could not be read`,
        message: `The file may be unreadable, or the archive may be damaged. Nothing was added to the mod list.`,
        detail,
      };
    case "writeFailed":
      return {
        title: `'${name}' could not be copied`,
        message:
          `The copy into the mod list failed, so the import was undone: no ${noun} and no icon were left behind. ` +
          `Check the free space and the permissions of the mod list folder.`,
        detail,
      };
    case "unknownModlist":
      return {
        title: `Mod list '${context.modlistName}' was not found`,
        message: `It may have been renamed or removed since this window was opened. Reselect it and import again.`,
        detail,
      };
    case "invalidModlistName":
      return {
        title: `Mod list '${context.modlistName}' cannot hold files`,
        message: `Its name is not usable as a folder name, so the launcher refuses to write inside it.`,
        detail,
      };
    case "invalidContentType":
      return {
        title: `'${context.contentType}' cannot be imported`,
        message: `The launcher imports local resource packs, data packs and shaders only.`,
        detail,
      };
  }
}
