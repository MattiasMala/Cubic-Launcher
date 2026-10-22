import type { AestheticGroup, FunctionalGroup, IncompatibilityRule, LinkRule, ModRow } from "../lib/types";

export function rebuildRowIds(rows: ModRow[]): ModRow[] {
  const rebuildAlternatives = (alternatives: ModRow[], ruleIndex: number, parentPath: string): ModRow[] =>
    alternatives.map((alt, altIndex) => {
      const namePart = alt.id.replace(/^rule-\d+(?:-alternative-\d+)*-/, "");
      const nextPath = `${parentPath}-alternative-${altIndex + 1}`;
      const nextId = `rule-${ruleIndex}${nextPath}-${namePart}`;
      const nextAlternatives = rebuildAlternatives(alt.alternatives ?? [], ruleIndex, nextPath);
      const unchanged =
        nextId === alt.id &&
        nextAlternatives.length === (alt.alternatives?.length ?? 0) &&
        nextAlternatives.every((candidate, index) => candidate === (alt.alternatives ?? [])[index]);

      return unchanged ? alt : { ...alt, id: nextId, alternatives: nextAlternatives };
    });

  return rows.map((row, ruleIndex) => {
    const namePart = row.id.replace(/^rule-\d+-/, "");
    const newId = `rule-${ruleIndex}-${namePart}`;
    const alternatives = rebuildAlternatives(row.alternatives ?? [], ruleIndex, "");
    const unchanged =
      newId === row.id &&
      alternatives.length === (row.alternatives?.length ?? 0) &&
      alternatives.every((candidate, index) => candidate === (row.alternatives ?? [])[index]);

    return unchanged ? row : { ...row, id: newId, alternatives };
  });
}

export function smartSetModRows(current: ModRow[], next: ModRow[]): ModRow[] {
  if (current.length === 0) return next;

  const byId = new Map(current.map(row => [row.id, row]));
  return next.map(nextRow => {
    const currentRow = byId.get(nextRow.id);
    if (!currentRow) return nextRow;

    const mergedAlternatives = smartSetModRows(currentRow.alternatives ?? [], nextRow.alternatives ?? []);
    const alternativesUnchanged =
      mergedAlternatives.length === (currentRow.alternatives?.length ?? 0) &&
      mergedAlternatives.every((alt, index) => alt === (currentRow.alternatives ?? [])[index]);

    if (
      currentRow.name === nextRow.name &&
      currentRow.kind === nextRow.kind &&
      currentRow.enabled === nextRow.enabled &&
      currentRow.area === nextRow.area &&
      currentRow.modrinth_id === nextRow.modrinth_id &&
      currentRow.iconImage === nextRow.iconImage &&
      currentRow.note === nextRow.note &&
      currentRow.tags.length === nextRow.tags.length &&
      currentRow.tags.every((tag, index) => tag === nextRow.tags[index]) &&
      alternativesUnchanged
    ) {
      return currentRow;
    }
    return { ...nextRow, alternatives: mergedAlternatives };
  });
}

export function findRowNamePath(rows: ModRow[], targetId: string, parentPath: string[] = []): string[] | null {
  for (const row of rows) {
    const path = [...parentPath, row.name];
    if (row.id === targetId) return path;
    const nested = findRowNamePath(row.alternatives ?? [], targetId, path);
    if (nested) return nested;
  }
  return null;
}

export function findRowIdByNamePath(rows: ModRow[], namePath: string[]): string | null {
  if (namePath.length === 0) return null;

  const [head, ...rest] = namePath;
  const row = rows.find(candidate => candidate.name === head);
  if (!row) return null;
  if (rest.length === 0) return row.id;
  return findRowIdByNamePath(row.alternatives ?? [], rest);
}

export function buildDefaultIconLabel(modlistName: string): string {
  const initials = modlistName
    .split(/\s+/)
    .filter(Boolean)
    .map(segment => segment[0])
    .join("")
    .slice(0, 3)
    .toUpperCase();

  return initials || "ML";
}

export function normalizeIdentifier(value: string): string {
  return value
    .split("")
    .map(character => /[a-z0-9]/i.test(character) ? character.toLowerCase() : "-")
    .join("")
    .replace(/^-+|-+$/g, "");
}

export function buildIdRemap(previousRows: ModRow[], nextRows: ModRow[]): Map<string, string> {
  const remap = new Map<string, string>();

  const collect = (previous: ModRow[], next: ModRow[]) => {
    previous.forEach((previousRow, index) => {
      const nextRow = next[index];
      if (!nextRow) return;
      remap.set(previousRow.id, nextRow.id);
      collect(previousRow.alternatives ?? [], nextRow.alternatives ?? []);
    });
  };

  collect(previousRows, nextRows);

  return remap;
}

export function reorderAlternativesInRows(rows: ModRow[], parentId: string, orderedAltIds: string[]): ModRow[] {
  return rows.map(row => {
    if (row.id === parentId && row.alternatives) {
      const altMap = new Map(row.alternatives.map(alt => [alt.id, alt]));
      const reordered = orderedAltIds.map(id => altMap.get(id)).filter(Boolean) as ModRow[];
      const unchanged =
        reordered.length === row.alternatives.length &&
        reordered.every((candidate, index) => candidate === row.alternatives![index]);
      return unchanged ? row : { ...row, alternatives: reordered };
    }

    const nextAlternatives = reorderAlternativesInRows(row.alternatives ?? [], parentId, orderedAltIds);
    const unchanged =
      nextAlternatives.length === (row.alternatives?.length ?? 0) &&
      nextAlternatives.every((candidate, index) => candidate === (row.alternatives ?? [])[index]);
    return unchanged ? row : { ...row, alternatives: nextAlternatives };
  });
}

/**
 * The order the editor draws one level in: every visual group at the place of
 * its first member, with all its members together in `blockIds` order; rows
 * outside groups keep their place. Rules saved in this order are tried in the
 * order the editor shows, and a drop "before the group" lands where it is
 * drawn (audit 101, editor-logica-8).
 */
export function contiguousGroupOrder(
  ids: readonly string[],
  groups: ReadonlyArray<{ blockIds: readonly string[] }>,
): string[] {
  const present = new Set(ids);
  const groupOf = new Map<string, readonly string[]>();
  for (const group of groups) {
    for (const id of group.blockIds) {
      if (!groupOf.has(id)) groupOf.set(id, group.blockIds);
    }
  }

  const ordered: string[] = [];
  const placed = new Set<string>();
  for (const id of ids) {
    if (placed.has(id)) continue;
    const members = groupOf.get(id);
    if (!members) {
      ordered.push(id);
      placed.add(id);
      continue;
    }
    for (const member of members) {
      if (present.has(member) && !placed.has(member) && groupOf.get(member) === members) {
        ordered.push(member);
        placed.add(member);
      }
    }
  }
  return ordered;
}

export function collectRowIds(rows: ModRow[]): Set<string> {
  const ids = new Set<string>();
  const collect = (items: ModRow[]) => {
    for (const row of items) {
      ids.add(row.id);
      if (row.alternatives?.length) collect(row.alternatives);
    }
  };
  collect(rows);
  return ids;
}

/**
 * Old row id → new row id, or `null` when the row is gone.
 *
 * Row ids are positional, so after a reorder or a delete every state keyed by
 * them has to go through the same translation (audit 101, `modlist-5`,
 * `editor-logica-3`). `removedIds` are the rows just deleted: their entries are
 * dropped, never remapped, even if a surviving row now has the same id.
 */
export type RowIdResolver = (id: string) => string | null;

export function rowIdResolver(
  idRemap: Map<string, string>,
  validIds: Set<string>,
  removedIds: ReadonlySet<string> = new Set(),
): RowIdResolver {
  return id => {
    if (removedIds.has(id)) return null;
    const next = idRemap.get(id) ?? id;
    return validIds.has(next) ? next : null;
  };
}

export function remapIds(ids: string[], resolve: RowIdResolver): string[] {
  return ids
    .map(resolve)
    .filter((id): id is string => id !== null)
    .filter((id, index, all) => all.indexOf(id) === index);
}

export function remapAestheticGroups(groups: AestheticGroup[], resolve: RowIdResolver): AestheticGroup[] {
  return groups
    // A group scoped to the alternatives of a deleted row goes with it, instead
    // of turning into a top-level group.
    .filter(group => !group.scopeRowId || resolve(group.scopeRowId) !== null)
    .map(group => ({
      ...group,
      blockIds: remapIds(group.blockIds, resolve),
      scopeRowId: group.scopeRowId ? resolve(group.scopeRowId) : group.scopeRowId,
    }));
}

export function remapFunctionalGroups(groups: FunctionalGroup[], resolve: RowIdResolver): FunctionalGroup[] {
  return groups.map(group => ({
    ...group,
    modIds: remapIds(group.modIds, resolve),
  }));
}

export function remapLinks(links: LinkRule[], resolve: RowIdResolver): LinkRule[] {
  return links.flatMap(link => {
    const fromId = resolve(link.fromId);
    const toId = resolve(link.toId);
    return fromId && toId ? [{ fromId, toId }] : [];
  });
}

export function remapIncompatibilities(rules: IncompatibilityRule[], resolve: RowIdResolver): IncompatibilityRule[] {
  return rules.flatMap(rule => {
    const winnerId = resolve(rule.winnerId);
    const loserId = resolve(rule.loserId);
    return winnerId && loserId ? [{ winnerId, loserId }] : [];
  });
}

/** Version rules and custom configs: both say which row they belong to in `modId`. */
export function remapByRowId<T extends { modId: string }>(items: T[], resolve: RowIdResolver): T[] {
  return items.flatMap(item => {
    const modId = resolve(item.modId);
    if (modId === null) return [];
    return [modId === item.modId ? item : { ...item, modId }];
  });
}

/** The row and every alternative below it, at any depth. */
export function collectSubtreeIds(row: ModRow, into: Set<string> = new Set()): Set<string> {
  into.add(row.id);
  for (const alt of row.alternatives ?? []) collectSubtreeIds(alt, into);
  return into;
}

/**
 * Drop the rows in `ids` wherever they sit in the tree (the backend deletes at
 * every depth, `editor_data.rs::delete_rules_from_root`). Untouched branches
 * keep their object identity.
 */
export function removeRowsDeep(rows: ModRow[], ids: ReadonlySet<string>): ModRow[] {
  let changed = false;
  const next: ModRow[] = [];
  for (const row of rows) {
    if (ids.has(row.id)) { changed = true; continue; }
    const alternatives = row.alternatives?.length ? removeRowsDeep(row.alternatives, ids) : row.alternatives;
    if (alternatives !== row.alternatives) {
      changed = true;
      next.push({ ...row, alternatives });
    } else {
      next.push(row);
    }
  }
  return changed ? next : rows;
}

/**
 * Set `enabled` on the rows in `ids` and on everything below them, at any
 * depth — what `toggle_rules_enabled_command` does on disk
 * (`editor_data.rs::set_enabled_recursive`).
 */
export function setEnabledDeep(rows: ModRow[], ids: ReadonlySet<string>, enabled: boolean): ModRow[] {
  const setSubtree = (row: ModRow): ModRow => {
    const alternatives = row.alternatives ?? [];
    const nextAlternatives = alternatives.map(setSubtree);
    const altsChanged = nextAlternatives.some((alt, index) => alt !== alternatives[index]);
    if (row.enabled === enabled && !altsChanged) return row;
    return { ...row, enabled, alternatives: altsChanged ? nextAlternatives : row.alternatives };
  };
  let changed = false;
  const next = rows.map(row => {
    let updated: ModRow;
    if (ids.has(row.id)) {
      updated = setSubtree(row);
    } else if (row.alternatives?.length) {
      const alternatives = setEnabledDeep(row.alternatives, ids, enabled);
      updated = alternatives === row.alternatives ? row : { ...row, alternatives };
    } else {
      updated = row;
    }
    if (updated !== row) changed = true;
    return updated;
  });
  return changed ? next : rows;
}

export function serializeGroupsLayout(groups: FunctionalGroup[]) {
  return JSON.stringify({
    tags: groups.map(group => ({
      id: group.id,
      name: group.name,
      tone: group.tone,
      modIds: group.modIds,
    })),
  });
}

export function serializeRuleGroups(groups: AestheticGroup[]): string {
  return JSON.stringify(
    groups.map(group => ({ id: group.id, name: group.name, collapsed: group.collapsed, rowIds: group.blockIds, scopeRowId: group.scopeRowId ?? null }))
  );
}

export function serializeExpandedRows(ids: string[]): string {
  return JSON.stringify([...ids].sort());
}

export function serializeLinks(links: Array<{ fromId: string; toId: string }>): string {
  return JSON.stringify([...links].sort((a, b) => `${a.fromId}|${a.toId}`.localeCompare(`${b.fromId}|${b.toId}`)));
}

export function serializeIncompatibilities(rules: Array<{ winnerId: string; loserId: string }>) {
  return JSON.stringify([...rules].sort((a, b) => `${a.winnerId}|${a.loserId}`.localeCompare(`${b.winnerId}|${b.loserId}`)));
}
