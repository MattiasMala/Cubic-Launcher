/**
 * Modrinth releases for one project on one target, for the "pin a version"
 * modal.
 *
 * The frontend already talks to Modrinth directly in three other places
 * (`add-mod-dialog/shared.ts`, `app/backend-loaders.ts`,
 * `mod-list-editor/use-content-tab-state.ts`); this follows the same shape and
 * the same User-Agent instead of adding a fourth convention.
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
    loaders: JSON.stringify([loader.toLowerCase()]),
    game_versions: JSON.stringify([mcVersion]),
  });

  const response = await fetch(
    `https://api.modrinth.com/v2/project/${encodeURIComponent(slug)}/version?${params}`,
    { headers: { "User-Agent": "CubicLauncher/0.1.0" } }
  );
  if (!response.ok) throw new Error(`Modrinth returned HTTP ${response.status}`);

  const payload: ModrinthVersionPayload[] = await response.json();

  return payload.map(version => ({
    id: version.id,
    versionNumber: version.version_number,
    versionType: version.version_type ?? "release",
  }));
}
