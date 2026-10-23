// Pure derived state for the UI.
//
// Selectors should not mutate signals, call Tauri commands, or perform
// persistence. If a computed value needs side effects, keep the read here and
// put the write path in `store-actions.ts` or a feature-level handler.

import { createMemo } from "solid-js";
import type { AccountSummary, FunctionalGroup, IncompatibilityRule, ModRow } from "./lib/types";
import {
  accounts,
  activeAccountId,
  aestheticGroups,
  alternativesPanelParentId,
  draftIncompatibilities,
  functionalGroups,
  incompatibilityFocusId,
  launchProgress,
  launchStageDetail,
  launchStageLabel,
  launchState,
  modListCards,
  modRowsState,
  savedIncompatibilities,
  savedLinks,
  search,
  selectedIds,
  selectedModListName,
  tagFilter,
  advancedPanelModId,
  updateCheckRunning,
} from "./store-state";

/**
 * From Play to the spawn: the pre-check and the launch work on the selection
 * Play was pressed with, so the version, the loader, the mod list and the
 * account cannot change under them (precheck-1).
 */
export const launchPreparing = () => updateCheckRunning() || launchState() === "resolving";

function collectRowsRecursive(rows: ModRow[], map: Map<string, ModRow>) {
  for (const row of rows) {
    map.set(row.id, row);
    if (row.alternatives?.length) collectRowsRecursive(row.alternatives, map);
  }
}

export const rowMap = createMemo(() => {
  const map = new Map<string, ModRow>();
  collectRowsRecursive(modRowsState(), map);
  return map;
});

export const selectedModList = createMemo(() =>
  modListCards().find(card => card.name === selectedModListName()) ?? modListCards()[0] ?? null
);

// No fallback to the first row: the launch uses the active row and nothing
// else, so with no active row the interface must not show one either
// (todo.md:1390).
export const activeAccount = createMemo(() =>
  accounts().find(account => account.id === activeAccountId()) ?? null
);

/**
 * The active account plays offline (D115), so the app makes no request of its
 * own (D140): no metadata, no search, no avatars. The backend checks the same
 * choice for the requests it makes, from the database.
 */
export const activePlaysOffline = createMemo(() => activeAccount()?.playsOffline ?? false);

/** An account's avatar, or none while the active account plays offline (D140): the initials stand in. */
export function accountAvatarUrl(account: AccountSummary | null | undefined): string | undefined {
  if (!account || activePlaysOffline()) return undefined;
  return account.avatarUrl;
}

/**
 * An image URL as the page may draw it: a remote one (the Modrinth CDN) only
 * while the active account plays online (D140). The icons fetched earlier in
 * the session stay in their maps, so switching to «Play offline» mid-session
 * no longer lets a row scrolled into view ask cdn.modrinth.com
 * (review-frontend-shell-28); a local `data:` or `asset:` icon is drawn
 * either way.
 */
export function offlineSafeImageUrl(url: string | null | undefined): string | undefined {
  if (!url) return undefined;
  if (activePlaysOffline() && /^https?:/i.test(url)) return undefined;
  return url;
}

export const selectedCount = createMemo(() => selectedIds().length);

export function rowOrDescendantMatchesTag(row: ModRow, fGroups: FunctionalGroup[], tf: Set<string>): boolean {
  if (fGroups.some(group => tf.has(group.id) && group.modIds.includes(row.id))) return true;
  return (row.alternatives ?? []).some(alt => rowOrDescendantMatchesTag(alt, fGroups, tf));
}

export type TopLevelItem =
  | { kind: "group"; id: string; name: string; collapsed: boolean; blocks: ModRow[] }
  | { kind: "row"; row: ModRow };

function sameRows(a: ModRow[], b: ModRow[]): boolean {
  return a.length === b.length && a.every((row, index) => row === b[index]);
}

/**
 * The editor's top level. `For` keys its children by object identity, so an
 * item is reused from the previous run whenever what it shows is the same
 * object: a row keeps its item while the row object is unchanged, a group
 * while its name, collapse state and block rows are. A new wrapper every run
 * made one change rebuild every row of the editor (audit 101,
 * r2-scala-mod-list-grandi-3).
 */
export const topLevelItems = createMemo<TopLevelItem[]>(previous => {
  const previousRowItems = new Map<ModRow, TopLevelItem>();
  const previousGroupItems = new Map<string, Extract<TopLevelItem, { kind: "group" }>>();
  for (const item of previous) {
    if (item.kind === "row") previousRowItems.set(item.row, item);
    else previousGroupItems.set(item.id, item);
  }

  const topGroups = aestheticGroups().filter(group => !group.scopeRowId);
  const groupByRow = new Map<string, string>();
  for (const group of topGroups) {
    for (const id of group.blockIds) groupByRow.set(id, group.id);
  }

  const query = search().trim().toLowerCase();
  const tags = tagFilter();
  const fGroups = functionalGroups();
  const rows = modRowsState();
  const rowById = new Map(rows.map(row => [row.id, row]));

  const addedGroups = new Set<string>();
  const items: TopLevelItem[] = [];

  for (const row of rows) {
    const groupId = groupByRow.get(row.id);
    if (groupId) {
      if (addedGroups.has(groupId)) continue;
      addedGroups.add(groupId);
      const group = topGroups.find(candidate => candidate.id === groupId);
      if (!group) continue;
      const blocks = group.blockIds
        .map(id => rowById.get(id))
        .filter((candidate): candidate is ModRow => Boolean(candidate))
        .filter(candidate => !query || rowMatchesQuery(candidate, query))
        .filter(candidate => tags.size === 0 || rowOrDescendantMatchesTag(candidate, fGroups, tags));
      if (blocks.length > 0 || (!query && tags.size === 0)) {
        const reusable = previousGroupItems.get(groupId);
        items.push(
          reusable && reusable.name === group.name && reusable.collapsed === group.collapsed && sameRows(reusable.blocks, blocks)
            ? reusable
            : { kind: "group", id: groupId, name: group.name, collapsed: group.collapsed, blocks },
        );
      }
      continue;
    }

    if (query && !rowMatchesQuery(row, query)) continue;
    if (tags.size > 0 && !rowOrDescendantMatchesTag(row, fGroups, tags)) continue;
    items.push(previousRowItems.get(row) ?? { kind: "row", row });
  }

  // Nothing changed: the same array, so nothing downstream reruns.
  return items.length === previous.length && items.every((item, index) => item === previous[index]) ? previous : items;
}, []);

export const activeLaunchStage = createMemo(() => ({
  label: launchStageLabel(),
  detail: launchStageDetail(),
  progress: launchProgress(),
}));

export const functionalGroupsByBlockId = createMemo(() => {
  const map = new Map<string, FunctionalGroup[]>();
  for (const group of functionalGroups()) {
    for (const id of group.modIds) {
      const current = map.get(id) ?? [];
      current.push(group);
      map.set(id, current);
    }
  }
  return map;
});

export const advancedPanelMod = createMemo(() => {
  const id = advancedPanelModId();
  return id ? (rowMap().get(id) ?? null) : null;
});

/**
 * Rows that are a pinned version, as the backend marks them (`row.pin`). A
 * local jar with an "Only when" is not a pin: inferring one from that shape
 * made "Remove pin" delete a jar the user uploaded by hand (audit 101,
 * modlist-2).
 */
export const pinnedRowIds = createMemo(() => {
  const ids = new Set<string>();
  for (const row of rowMap().values()) {
    if (row.pin) ids.add(row.id);
  }
  return ids;
});

/** Top-level rows, the only ones a version can be pinned on (plus the dynamic entry of a pin). */
export const topLevelRowIds = createMemo(() => new Set(modRowsState().map(row => row.id)));

export const parentIdByChildId = createMemo(() => {
  const map = new Map<string, string>();
  function walk(rows: ModRow[], parentId: string | null) {
    for (const row of rows) {
      if (parentId !== null) map.set(row.id, parentId);
      walk(row.alternatives ?? [], row.id);
    }
  }
  walk(modRowsState(), null);
  return map;
});

export const conflictModIds = createMemo(() => {
  const ids = new Set<string>();
  for (const rule of savedIncompatibilities()) {
    ids.add(rule.winnerId);
    ids.add(rule.loserId);
  }
  return ids;
});

export const conflictPairsForId = createMemo(() => {
  const byId = new Map<string, Array<{ winnerId: string; loserId: string }>>();
  for (const rule of savedIncompatibilities()) {
    for (const id of [rule.winnerId, rule.loserId]) {
      const list = byId.get(id) ?? [];
      list.push(rule);
      byId.set(id, list);
    }
  }
  return byId;
});

export const linksByModId = createMemo(() => {
  const map = new Map<string, Array<{ partnerId: string; direction: "mutual" | "requires" | "required-by" }>>();
  const links = savedLinks();
  const fromTo = new Set(links.map(link => `${link.fromId}|${link.toId}`));
  for (const link of links) {
    const reverse = fromTo.has(`${link.toId}|${link.fromId}`);
    const fromList = map.get(link.fromId) ?? [];
    if (!fromList.some(entry => entry.partnerId === link.toId)) {
      fromList.push({ partnerId: link.toId, direction: reverse ? "mutual" : "requires" });
      map.set(link.fromId, fromList);
    }
    if (!reverse) {
      const toList = map.get(link.toId) ?? [];
      if (!toList.some(entry => entry.partnerId === link.fromId)) {
        toList.push({ partnerId: link.fromId, direction: "required-by" });
        map.set(link.toId, toList);
      }
    }
  }
  return map;
});

export function findDirectParentId(rows: ModRow[], targetId: string): string | null {
  for (const row of rows) {
    if ((row.alternatives ?? []).some(alt => alt.id === targetId)) return row.id;
    const nested = findDirectParentId(row.alternatives ?? [], targetId);
    if (nested) return nested;
  }
  return null;
}

export const tagFilterForcedExpanded = createMemo(() => {
  const tags = tagFilter();
  const query = search().trim().toLowerCase();
  if (tags.size === 0 && !query) return new Set<string>();
  const fGroups = functionalGroups();

  const anyDescendantMatchesTag = (row: ModRow): boolean =>
    (row.alternatives ?? []).some(alt =>
      fGroups.some(group => tags.has(group.id) && group.modIds.includes(alt.id)) ||
      anyDescendantMatchesTag(alt)
    );

  const anyDescendantMatchesSearch = (row: ModRow): boolean =>
    (row.alternatives ?? []).some(alt =>
      alt.name.toLowerCase().includes(query) || anyDescendantMatchesSearch(alt)
    );

  const forced = new Set<string>();
  const processRow = (row: ModRow) => {
    if (tags.size > 0 && anyDescendantMatchesTag(row)) forced.add(row.id);
    if (query && !row.name.toLowerCase().includes(query) && anyDescendantMatchesSearch(row)) forced.add(row.id);
    for (const alt of row.alternatives ?? []) processRow(alt);
  };
  for (const row of modRowsState()) processRow(row);
  return forced;
});

export const alternativesPanelParent = createMemo(() => {
  const id = alternativesPanelParentId();
  return id ? (rowMap().get(id) ?? null) : null;
});

export const focusedIncompatibilityMod = createMemo(() => {
  const id = incompatibilityFocusId();
  return id ? rowMap().get(id) ?? null : null;
});

export const priorityParadoxDetected = createMemo(() =>
  createsPriorityParadox(draftIncompatibilities(), modRowsState().map(row => row.id))
);

export const selectedTopLevelId = createMemo(() => {
  const ids = selectedIds();
  return ids.length === 1 ? ids[0] : null;
});

export function wait(ms: number) {
  return new Promise<void>(resolve => setTimeout(resolve, ms));
}

export function rowMatchesQuery(row: ModRow, query: string): boolean {
  if (row.name.toLowerCase().includes(query)) return true;
  return (row.alternatives ?? []).some(alt => rowMatchesQuery(alt, query));
}

const TAG_BASE_CLASS = "inline-flex items-center gap-0.5 rounded-md border px-1.5 py-0.5 text-[10px]";

const LEGACY_TONE_CLASSES: Record<string, string> = {
  violet: `${TAG_BASE_CLASS} border-primary/30 bg-primary/15 text-primary`,
  sky: `${TAG_BASE_CLASS} border-sky-500/30 bg-sky-500/15 text-sky-300`,
  amber: `${TAG_BASE_CLASS} border-amber-500/30 bg-amber-500/15 text-amber-300`,
};

const LEGACY_TONE_HUES: Record<string, number> = { violet: 270, sky: 200, amber: 38 };

export function functionalGroupTagClass(tone: string): string {
  return LEGACY_TONE_CLASSES[tone] ?? TAG_BASE_CLASS;
}

export function functionalGroupTagStyle(tone: string): string {
  if (LEGACY_TONE_CLASSES[tone]) return "";
  const hue = parseInt(tone, 10);
  if (Number.isNaN(hue)) return "";
  return `border-color: hsl(${hue} 60% 50% / 0.3); background-color: hsl(${hue} 60% 50% / 0.15); color: hsl(${hue} 70% 65%);`;
}

export function toneToHue(tone: string): number {
  const parsed = parseInt(tone, 10);
  return LEGACY_TONE_HUES[tone] ?? (Number.isNaN(parsed) ? 270 : parsed);
}

export function huePreviewColor(hue: number): string {
  return `hsl(${hue} 70% 55%)`;
}

function createsPriorityParadox(rules: IncompatibilityRule[], allIds: string[]): boolean {
  const adjacency = new Map<string, string[]>();
  for (const rule of rules) {
    const edges = adjacency.get(rule.winnerId) ?? [];
    edges.push(rule.loserId);
    adjacency.set(rule.winnerId, edges);
  }
  const visited = new Set<string>();
  const stack = new Set<string>();

  function dfs(node: string): boolean {
    if (stack.has(node)) return true;
    if (visited.has(node)) return false;
    visited.add(node);
    stack.add(node);
    for (const neighbour of adjacency.get(node) ?? []) {
      if (dfs(neighbour)) return true;
    }
    stack.delete(node);
    return false;
  }

  for (const id of allIds) {
    if (!visited.has(id) && dfs(id)) return true;
  }
  return false;
}
