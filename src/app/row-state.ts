import type { AestheticGroup, FunctionalGroup, ModRow } from "../lib/types";

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

export function renameRowId(rowId: string, newName: string): string {
  const alternativeMatch = rowId.match(/^(rule-\d+(?:-alternative-\d+)*)-/);
  if (alternativeMatch) return `${alternativeMatch[1]}-${normalizeIdentifier(newName)}`;

  const ruleMatch = rowId.match(/^(rule-\d+)-/);
  if (ruleMatch) return `${ruleMatch[1]}-${normalizeIdentifier(newName)}`;

  return rowId;
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

export function remapIds(ids: string[], idRemap: Map<string, string>, validIds: Set<string>): string[] {
  return ids
    .map(id => idRemap.get(id) ?? id)
    .filter(id => validIds.has(id))
    .filter((id, index, all) => all.indexOf(id) === index);
}

export function remapAestheticGroups(groups: AestheticGroup[], idRemap: Map<string, string>, validIds: Set<string>): AestheticGroup[] {
  return groups.map(group => ({
    ...group,
    blockIds: remapIds(group.blockIds, idRemap, validIds),
    scopeRowId: group.scopeRowId ? (idRemap.get(group.scopeRowId) ?? group.scopeRowId) : group.scopeRowId,
  }));
}

export function remapFunctionalGroups(groups: FunctionalGroup[], idRemap: Map<string, string>, validIds: Set<string>): FunctionalGroup[] {
  return groups.map(group => ({
    ...group,
    modIds: remapIds(group.modIds, idRemap, validIds),
  }));
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
