// The rule for a NEW mod-list name, mirrored from `validate_new_modlist_name`
// in src-tauri/src/path_safety.rs (windows-5, r2-percorsi-non-ascii-e-limiti-windows-1).
// The backend is the one that decides; this only lets the Create dialog say
// why before the user presses the button. Existing names are never checked.

const FORBIDDEN = ['<', '>', ':', '"', '/', '\\', '|', '?', '*', ';'];
const RESERVED = new Set([
  "CON", "PRN", "AUX", "NUL",
  ...["0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "¹", "²", "³"].flatMap(n => [`COM${n}`, `LPT${n}`]),
]);

/** Why `name` (already trimmed) cannot be a new mod list, or null when it can. */
export function modlistNameProblem(name: string): string | null {
  if (!name) return null;
  const found = [...name].find(c => FORBIDDEN.includes(c) || c.charCodeAt(0) < 32 || c.charCodeAt(0) === 127);
  if (found !== undefined) {
    const shown = FORBIDDEN.includes(found) ? `"${found}"` : "a control character";
    return `The name contains ${shown}. A mod list name cannot contain any of < > : " / \\ | ? * ;`;
  }
  if (name.endsWith(".") || name.endsWith(" ")) return "A mod list name cannot end with a dot or a space.";
  if (name === "." || name === "..") return "A mod list name cannot be . or ..";
  const stem = name.split(".")[0].replace(/ +$/, "");
  if (RESERVED.has(stem.toUpperCase())) return `"${stem}" is a name Windows reserves for a device.`;
  if (name.startsWith(".cubic-")) return "Names starting with .cubic- are reserved by the launcher.";
  return null;
}
