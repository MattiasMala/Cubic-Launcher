import { invoke } from "@tauri-apps/api/core";

/**
 * Every instance directory of each named mod list, sorted.
 *
 * Both world screens need it — the home's ⋮ menu and the «Worlds» tab of the
 * mod list settings — because since D99 a world can be shared with an
 * instance of any mod list. It reads the same instance roots the mod list's
 * file browser reads: an instance with no `saves/` is a perfectly good
 * destination, so the worlds themselves cannot be the source.
 *
 * A mod list whose instances cannot be read offers nothing, which is a
 * smaller failure than a banner.
 */
export async function loadInstancesByModlist(
  modlistNames: string[],
): Promise<Record<string, string[]>> {
  const byModlist: Record<string, string[]> = {};
  for (const modlistName of modlistNames) {
    try {
      const nodes = await invoke<{ name: string; isDir: boolean }[]>(
        "list_instance_files_command",
        { modlistName, relativePath: null },
      );
      byModlist[modlistName] = nodes.filter(node => node.isDir).map(node => node.name).sort();
    } catch {
      byModlist[modlistName] = [];
    }
  }
  return byModlist;
}
