import { version } from "../../package.json";

/**
 * Who the launcher says it is to Modrinth from the frontend: the same string
 * the backend sends (`modrinth::USER_AGENT` in `src-tauri/src/modrinth.rs`).
 * The version is read from `package.json` at build time, not written by hand,
 * and the URL is the repository Modrinth can actually reach.
 */
export const MODRINTH_HEADERS = {
  "User-Agent": `cubic-launcher/${version} (https://github.com/MattiasMala/Cubic-Launcher)`,
};
