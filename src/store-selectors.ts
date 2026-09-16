// Pure derived state for the UI.
//
// Selectors should not mutate signals, call Tauri commands, or perform
// persistence. If a computed value needs side effects, keep the read here and
// put the write path in `store-actions.ts` or a feature-level handler.

import { createMemo } from "solid-js";
import type { FunctionalGroup, IncompatibilityRule, ModRow } from "./lib/types";
import {
  downloadItems,
  accounts,
  activeAccountId,
  aestheticGroups,
  alternativesPanelParentId,
  draftIncompatibilities,
  functionalGroups,
  incompatibilityFocusId,
  launchProgress,
  launcherErrors,
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
  sortOrder,
  tagFilter,
  advancedPanelModId,
  versionRules,
} from "./store-state";

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

export const activeAccount = createMemo(() =>
  accounts().find(account => account.id === activeAccountId()) ?? accounts()[0] ?? null
);

export const selectedCount = createMemo(() => selectedIds().length);

export function rowOrDescendantMatchesTag(row: ModRow, fGroups: FunctionalGroup[], tf: Set<string>): boolean {
  if (fGroups.some(group => tf.has(group.id) && group.modIds.includes(row.id))) return true;
  return (row.alternatives ?? []).some(alt => rowOrDescendantMatchesTag(alt, fGroups, tf));
}

export type TopLevelItem =
  | { kind: "group"; id: string; name: string; collapsed: boolean; blocks: ModRow[] }
  | { kind: "row"; row: ModRow };

export const topLevelItems = createMemo<TopLevelItem[]>(() => {
  const topGroups = aestheticGroups().filter(group => !group.scopeRowId);
  const groupByRow = new Map<string, string>();
  for (const group of topGroups) {
    for (const id of group.blockIds) groupByRow.set(id, group.id);
  }

  const query = search().trim().toLowerCase();
  const tags = tagFilter();
  const order = sortOrder();
  const fGroups = functionalGroups();

  const addedGroups = new Set<string>();
  const items: TopLevelItem[] = [];
  const ungroupedRows: ModRow[] = [];

  for (const row of modRowsState()) {
    const groupId = groupByRow.get(row.id);
    if (groupId) {
      if (addedGroups.has(groupId)) continue;
      addedGroups.add(groupId);
      const group = topGroups.find(candidate => candidate.id === groupId);
      if (!group) continue;
      let blocks = group.blockIds
        .map(id => modRowsState().find(candidate => candidate.id === id))
        .filter((candidate): candidate is ModRow => Boolean(candidate))
        .filter(candidate => !query || rowMatchesQuery(candidate, query))
        .filter(candidate => tags.size === 0 || rowOrDescendantMatchesTag(candidate, fGroups, tags));
      if (order === "name-az") blocks = [...blocks].sort((a, b) => a.name.localeCompare(b.name));
      if (order === "name-za") blocks = [...blocks].sort((a, b) => b.name.localeCompare(a.name));
      if (blocks.length > 0 || (!query && tags.size === 0)) {
        items.push({ kind: "group", id: groupId, name: group.name, collapsed: group.collapsed, blocks });
      }
      continue;
    }

    if (query && !rowMatchesQuery(row, query)) continue;
    if (tags.size > 0 && !rowOrDescendantMatchesTag(row, fGroups, tags)) continue;
    ungroupedRows.push(row);
    items.push({ kind: "row", row });
  }

  if (order !== "default") {
    const sorted = [...ungroupedRows].sort((a, b) =>
      order === "name-az" ? a.name.localeCompare(b.name) : b.name.localeCompare(a.name)
    );
    let index = 0;
    return items.map(item => item.kind === "row" ? ({ kind: "row", row: sorted[index++] } as TopLevelItem) : item);
  }

  return items;
});

export const filteredRows = createMemo(() => {
  const query = search().trim().toLowerCase();
  const tags = tagFilter();
  const order = sortOrder();
  const fGroups = functionalGroups();
  let rows = modRowsState();
  if (query) rows = rows.filter(row => rowMatchesQuery(row, query));
  if (tags.size > 0) rows = rows.filter(row => rowOrDescendantMatchesTag(row, fGroups, tags));
  if (order === "name-az") rows = [...rows].sort((a, b) => a.name.localeCompare(b.name));
  if (order === "name-za") rows = [...rows].sort((a, b) => b.name.localeCompare(a.name));
  return rows;
});

export const manualModRows = createMemo(() => {
  const alternatives = modRowsState().flatMap(row => (row.alternatives ?? []).filter(alt => alt.kind === "local"));
  return [...modRowsState().filter(row => row.kind === "local"), ...alternatives];
});

export const manualModWarningCount = createMemo(() => manualModRows().length);

export const completedDownloads = createMemo(() => downloadItems().filter(item => item.status === "complete"));

export const latestError = createMemo(() => launcherErrors()[0] ?? null);

export const activeLaunchStage = createMemo(() => ({
  label: launchStageLabel(),
  detail: launchStageDetail(),
  progress: launchProgress(),
}));

export const playButtonLabel = createMemo(() => {
  if (launchState() === "resolving") return `Resolving ${launchProgress()}%`;
  if (launchState() === "ready") return "Launch Ready";
  return "Play";
});

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
 * Rows that are a pinned version: a local entry carrying an `only` version
 * rule. That rule is what the pin writes, and it is what tells a pin apart
 * from a jar the user uploaded by hand — whose jar must never be deleted.
 */
export const pinnedRowIds = createMemo(() => {
  const rows = rowMap();
  return new Set(
    versionRules()
      .filter(rule => rule.kind === "only" && rows.get(rule.modId)?.kind === "local")
      .map(rule => rule.modId)
  );
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

export const groupNameByBlockId = createMemo(() => {
  const map = new Map<string, string>();
  for (const group of aestheticGroups()) {
    for (const id of group.blockIds) map.set(id, group.name);
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

export const filteredGroupSections = createMemo(() => {
  const query = search().trim().toLowerCase();
  const tags = tagFilter();
  const order = sortOrder();
  const fGroups = functionalGroups();
  return aestheticGroups().filter(group => !group.scopeRowId).map(group => {
    let blocks = group.blockIds
      .map(id => rowMap().get(id))
      .filter((row): row is ModRow => Boolean(row))
      .filter(row => !query || rowMatchesQuery(row, query))
      .filter(row => tags.size === 0 || rowOrDescendantMatchesTag(row, fGroups, tags));
    if (order === "name-az") blocks = [...blocks].sort((a, b) => a.name.localeCompare(b.name));
    if (order === "name-za") blocks = [...blocks].sort((a, b) => b.name.localeCompare(a.name));
    return { ...group, blocks };
  });
});

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

export function resolveToTopLevelId(rowId: string): string {
  const match = rowId.match(/^(rule-\d+)-alternative-/);
  if (!match) return rowId;
  const prefix = `${match[1]}-`;
  for (const row of modRowsState()) {
    if (row.id.startsWith(prefix) && !row.id.includes("-alternative-")) return row.id;
  }
  return rowId;
}

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

export function moveItemBefore<T extends { id: string }>(items: T[], draggedId: string, targetId: string): T[] {
  const dragged = items.find(item => item.id === draggedId);
  if (!dragged) return items;
  const rest = items.filter(item => item.id !== draggedId);
  const idx = rest.findIndex(item => item.id === targetId);
  if (idx < 0) return items;
  rest.splice(idx, 0, dragged);
  return rest;
}
