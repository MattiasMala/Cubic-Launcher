import { MODRINTH_HEADERS } from "./modrinth-user-agent";

/**
 * Modrinth releases for one project on one target, for the "pin a version"
 * modal.
 *
 * The frontend already talks to Modrinth directly in three other places
 * (`add-mod-dialog/shared.ts`, `app/backend-loaders.ts`,
 * `lib/content-meta.ts`); this follows the same shape and sends the same
 * `MODRINTH_HEADERS` instead of adding a fourth convention.
 */

export type PinRelease = {
  /** Modrinth version id — what the pin command needs. */
  id: string;
  versionNumber: string;
  versionType: string;
};

type ModrinthVersionPayload = {
  id: string;
  version_number: string;
  version_type?: string;
  date_published?: string;
};

/**
 * The loaders a Modrinth lookup for this target accepts — the same rule as
 * `ResolutionTarget::modrinth_query_loaders` in the backend (D148): NeoForge
 * on Minecraft 1.20.1 is the fork of Forge 47.1 and loads Forge 1.20.1 mods,
 * so its list carries the `forge` builds too. Every other target asks for its
 * own loader only.
 */
export function modrinthQueryLoaders(mcVersion: string, loader: string): string[] {
  const own = loader.toLowerCase();
  return own === "neoforge" && mcVersion.trim() === "1.20.1" ? ["neoforge", "forge"] : [own];
}

/**
 * Releases of `slug` compatible with `mcVersion` + `loader`, newest first.
 *
 * The loader filter is never omitted: an absent filter is not "no filter
 * wanted" but "no filter", and the answer would carry builds for other
 * loaders.
 */
export async function fetchPinReleases(
  slug: string,
  mcVersion: string,
  loader: string
): Promise<PinRelease[]> {
  const params = new URLSearchParams({
    loaders: JSON.stringify(modrinthQueryLoaders(mcVersion, loader)),
    game_versions: JSON.stringify([mcVersion]),
  });

  const response = await fetch(
    `https://api.modrinth.com/v2/project/${encodeURIComponent(slug)}/version?${params}`,
    { headers: MODRINTH_HEADERS }
  );
  if (!response.ok) throw new Error(`Modrinth returned HTTP ${response.status}`);

  const payload: ModrinthVersionPayload[] = await response.json();

  return payload.map(version => ({
    id: version.id,
    versionNumber: version.version_number,
    versionType: version.version_type ?? "release",
  }));
}
