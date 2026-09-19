import { createSignal } from "solid-js";

/**
 * What Modrinth says about one content-pack project, cached for the whole app.
 *
 * It used to be a private name cache inside the content tab, which was enough
 * while the only consumer was the tab's own rows. Two things need it now: the
 * update popup, whose pack rows must not borrow `modNames()`/`modIcons()` —
 * those are keyed by *mod* project and filled by the mod pipeline — and the
 * compatibility badge on the editor row (D62).
 */
export type ContentProjectMeta = {
  name?: string;
  iconUrl?: string;
  /**
   * Every game version the project publishes something for, as
   * `GET /v2/projects` returns it — the union over its versions.
   */
  gameVersions?: string[];
  /**
   * The request went out and this id came back with nothing, or did not come
   * back at all.
   *
   * The third state the badge needs: "nobody asked yet" must not read as
   * "could not ask" (D63), and neither may read as "there is no version for
   * this target" (D60).
   */
  lookupFailed?: boolean;
};

const [contentProjects, setContentProjects] = createSignal<Map<string, ContentProjectMeta>>(new Map());

export { contentProjects };

/**
 * A readable title known before any request — the add dialog has it, and the
 * row should not flash a raw slug while the fetch is in flight.
 */
export function seedContentName(id: string, name: string) {
  if (!id || !name || name === id) return;
  setContentProjects(current => {
    const next = new Map(current);
    next.set(id, { ...next.get(id), name });
    return next;
  });
}

/**
 * Fill the cache for these entry ids with **one** request.
 *
 * The same endpoint the content tab has always called; it is asked for the
 * ids that have no `gameVersions` yet, so a second tab open is free and a
 * previous failure is retried instead of being remembered as "unknown
 * forever".
 */
export async function fetchContentProjects(ids: string[]): Promise<void> {
  const missing = ids.filter(id => contentProjects().get(id)?.gameVersions === undefined);
  if (missing.length === 0) return;

  let projects: Array<{
    id: string;
    slug: string;
    title: string;
    icon_url?: string | null;
    game_versions?: string[];
  }> = [];

  try {
    const param = encodeURIComponent(JSON.stringify(missing));
    const response = await fetch(`https://api.modrinth.com/v2/projects?ids=${param}`, {
      headers: { "User-Agent": "CubicLauncher/0.1.0" },
    });
    if (response.ok) projects = await response.json();
  } catch {
    // Metadata is best effort, and the failure is recorded below rather than
    // swallowed: an entry nobody could ask about says so.
  }

  setContentProjects(current => {
    const next = new Map(current);
    const seen = new Set<string>();
    for (const project of projects) {
      const meta: ContentProjectMeta = {
        name: project.title || undefined,
        iconUrl: project.icon_url ?? undefined,
        gameVersions: project.game_versions ?? [],
      };
      if (project.slug) { next.set(project.slug, meta); seen.add(project.slug); }
      if (project.id) { next.set(project.id, meta); seen.add(project.id); }
    }
    // Asked for and not answered: the request failed, or Modrinth does not
    // know this id any more.
    for (const id of missing) {
      if (seen.has(id)) continue;
      next.set(id, { ...next.get(id), gameVersions: undefined, lookupFailed: true });
    }
    return next;
  });
}

export function mcVersionMatches(pattern: string, concrete: string): boolean {
  if (pattern === concrete) return true;
  const lower = pattern.toLowerCase();
  if (!lower.endsWith(".x")) return false;
  const prefix = pattern.slice(0, -2);
  return concrete.startsWith(prefix) && concrete[prefix.length] === ".";
}

export type ContentCompatibility = "compatible" | "no-version" | "unknown" | "pending";

/**
 * Whether Modrinth publishes anything for this target, from the project's own
 * `game_versions` — the union over its versions, which is the question D60
 * asks.
 *
 * It is **not** the same code path the backend takes, and the difference is
 * worth knowing: `fetch_content_pack_versions` sends
 * `game_versions=["1.20.1"]` and Modrinth filters server-side by exact
 * string, while this reads the union the project object already carries. The
 * two agreed on all five of the real list's packs (measured 2026-09-19,
 * `visual-effects-plus` false on both sides, the other four true). The `.x`
 * branch of `mcVersionMatches` never fires here, since Modrinth returns
 * concrete versions; it is there because the same helper reads the mod-list's
 * own version rules, which do use wildcards.
 *
 * Loader is not part of it, on purpose: content packs are `loaders:
 * ["minecraft"]` and the backend's pack lookup filters by game version alone,
 * so a loader here would make the badge stricter than the launch.
 *
 * `pending` is the state before anyone has asked, and it shows nothing: a
 * badge that appears for half a second on every row and then goes away is
 * worse than no badge, and it is not what D63 is about.
 */
export function contentCompatibility(
  meta: ContentProjectMeta | undefined,
  mcVersion: string,
): ContentCompatibility {
  if (meta?.lookupFailed) return "unknown";
  const gameVersions = meta?.gameVersions;
  if (gameVersions === undefined) return "pending";
  return gameVersions.some(candidate => mcVersionMatches(candidate, mcVersion))
    ? "compatible"
    : "no-version";
}
