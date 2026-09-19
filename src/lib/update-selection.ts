import type { ContentUpdateRow, ModUpdateRow } from "./types";

/**
 * The version map the launch receives after the popup, built from the
 * pre-check's `resolved` (D16).
 *
 * `resolved` already carries the **candidate** version for every mod that has
 * an update, so an accepted row needs no work: the map is right as it stands.
 * Only a **refused** row is overwritten, back to the version currently
 * registered in `mod_cache`.
 *
 * Inverting that — writing the candidate for the accepted rows over a map of
 * current versions — would install the updates the user refused, silently and
 * against the list they just read. Hence the direction here, and the rows that
 * are not in `accepted` being the ones that move.
 *
 * "Skip" is the empty `accepted` set: every row falls back to its current
 * version. Mods without a row are untouched either way — they have one
 * version, the one `resolved` names.
 */
export function buildResolvedVersions(
  resolved: Record<string, string>,
  updates: readonly ModUpdateRow[],
  accepted: ReadonlySet<string>,
): Record<string, string> {
  const final: Record<string, string> = { ...resolved };
  for (const row of updates) {
    if (!accepted.has(row.modId)) final[row.modId] = row.currentVersionId;
  }
  return final;
}

/**
 * The key a content row is accepted or refused under.
 *
 * Category and entry id together, because an entry id is unique only inside
 * its category — the same reason the backend's map is nested and not flat.
 */
export function contentRowKey(row: { category: string; entryId: string }): string {
  return `${row.category}/${row.entryId}`;
}

/**
 * The pack half of the same decision, with the same direction and for the same
 * reason: only the **refused** rows move, back to the version that is
 * installed right now.
 *
 * Rows of a category whose notification checkbox is off are simply not in
 * `accepted`, so they are refused here and the launch keeps what is on disk —
 * D30's rule applied per category instead of globally. An entry that was never
 * installed has no row at all and stays at its candidate in `resolvedContent`,
 * so switching a category off never blocks a **first** install (D17): "do not
 * update" is not "do not install".
 */
export function buildResolvedContent(
  resolvedContent: Record<string, Record<string, string>>,
  contentUpdates: readonly ContentUpdateRow[],
  accepted: ReadonlySet<string>,
): Record<string, Record<string, string>> {
  const final: Record<string, Record<string, string>> = {};
  for (const [category, entries] of Object.entries(resolvedContent)) {
    final[category] = { ...entries };
  }
  for (const row of contentUpdates) {
    if (accepted.has(contentRowKey(row))) continue;
    (final[row.category] ??= {})[row.entryId] = row.currentVersionId;
  }
  return final;
}
