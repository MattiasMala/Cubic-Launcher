import { batch, createMemo } from "solid-js";
import type { AestheticGroup, ModRow } from "../../lib/types";
import {
  aestheticGroups,
  editingGroupId,
  functionalGroups,
  groupNameDraft,
  modIcons,
  modRowsState,
  removeAestheticGroup,
  rowMatchesQuery,
  search,
  setAestheticGroups,
  setGroupNameDraft,
  setModRowsState,
  startGroupRename,
  commitGroupRename,
  tagFilter,
  toggleGroupCollapsed,
} from "../../store";
import { useDragEngine, type DragItem } from "../../lib/dragEngine";
import {
  computePreviewTranslates,
  findRowByIdDeep,
  updateAltsDeep,
} from "../../lib/dragUtils";
import type { AltSectionProps, AltTLItem } from "./types";
import { altTlId } from "./types";

const removeFrom = (ids: string[], id: string) => ids.filter(i => i !== id);

const insertBefore = (ids: string[], id: string, beforeId: string) => {
  const next = removeFrom(ids, id);
  const idx = next.findIndex(i => i === beforeId);
  if (idx < 0) return [...next, id];
  next.splice(idx, 0, id);
  return next;
};

export function useAltSectionDrag(props: AltSectionProps) {
  let containerRef: HTMLDivElement | undefined;

  const setContainerRef = (el: HTMLDivElement | undefined) => {
    containerRef = el;
  };

  const altMatchesFilter = (alt: ModRow): boolean => {
    const q = search().trim().toLowerCase();
    const tf = tagFilter();
    if (q && !rowMatchesQuery(alt, q)) return false;
    if (tf.size === 0) return true;
    const fg = functionalGroups();
    const check = (row: ModRow): boolean =>
      fg.some(g => tf.has(g.id) && g.modIds.includes(row.id)) ||
      (row.alternatives ?? []).some(check);
    return check(alt);
  };

  const scopedAltGroups = () => {
    return aestheticGroups()
      .filter(g => g.scopeRowId === props.parentRow.id)
      .map(g => {
        const blocks = g.blockIds
          .map(id => (props.parentRow.alternatives ?? []).find(a => a.id === id))
          .filter((a): a is ModRow => Boolean(a))
          .filter(altMatchesFilter);
        return { ...g, blocks };
      })
      .filter(g => g.blocks.length > 0);
  };

  const altGroupMembership = () => {
    const map = new Map<string, string>();
    for (const g of aestheticGroups().filter(g => g.scopeRowId === props.parentRow.id)) {
      for (const id of g.blockIds) map.set(id, g.id);
    }
    return map;
  };

  const altTopLevelItems = (): AltTLItem[] => {
    const membership = altGroupMembership();
    const groups = scopedAltGroups();
    const groupMap = new Map(groups.map(g => [g.id, g]));
    const seenGroups = new Set<string>();
    const items: AltTLItem[] = [];

    for (const alt of (props.parentRow.alternatives ?? [])) {
      const gId = membership.get(alt.id);
      if (gId) {
        if (!seenGroups.has(gId)) {
          seenGroups.add(gId);
          const group = groupMap.get(gId);
          if (group) {
            items.push({
              kind: "alt-group",
              id: group.id,
              name: group.name,
              collapsed: group.collapsed,
              blocks: group.blocks,
              blockIds: group.blockIds,
            });
          }
        }
      } else if (altMatchesFilter(alt)) {
        items.push({ kind: "alt-row", row: alt });
      }
    }
    return items;
  };

  const engine = useDragEngine({
    containerRef: () => containerRef,
    getItems: () => altTopLevelItems().map((item): DragItem =>
      item.kind === "alt-row"
        ? { kind: "row", id: item.row.id }
        : { kind: "group", id: item.id }
    ),
    onCommit: (fromId, dropId, fromKind) => {
      if (fromId === dropId) return;
      if (fromKind === "group") {
        commitAltGroupDrop(fromId, dropId);
      } else {
        commitAltDrop(fromId, dropId);
      }
    },
    detect: cursorY => detectAltDropTarget(cursorY),
  });

  const isGroupDrag = () => engine.draggingKind() === "group";

  const draggingAlt = () =>
    (props.parentRow.alternatives ?? []).find(a => a.id === engine.draggingId()) ?? null;

  const draggingAltGroupData = () =>
    scopedAltGroups().find(g => g.id === engine.draggingId()) ?? null;

  const detectAltDropTarget = (cursorY: number): string | null => {
    const items = altTopLevelItems();
    const heights = engine.cachedHeights();
    const tops = engine.cachedTops();
    const groupDrag = isGroupDrag();
    const draggingItemId = engine.draggingId()!;
    const membership = altGroupMembership();
    const initMidYs = engine.cachedMidYs();
    // Measurements are from dragStart: if the list scrolled since, rows and
    // their centres moved up by scrollDelta.
    const scrollDelta = engine.scrollDelta();
    const midOf = (rowId: string) => {
      const mid = initMidYs.get(rowId);
      return mid === undefined ? undefined : mid - scrollDelta;
    };

    const dragTlId = groupDrag ? `alt-group:${draggingItemId}` : draggingItemId;
    for (const item of items) {
      const id = altTlId(item);
      if (id === dragTlId) continue;

      const rawY = tops.get(id);
      const h = heights.get(id) ?? 40;
      if (rawY === undefined) continue;
      const y = rawY - scrollDelta;

      if (cursorY < y + h) {
        if (item.kind === "alt-group") {
          const gId = item.id;
          const relY = cursorY - y;

          if (groupDrag) {
            return cursorY >= y + h / 2 ? `alt-group-after:${gId}` : `alt-group:${gId}`;
          }

          const draggedIsGroupMember = membership.get(draggingItemId) === gId;

          if (!draggedIsGroupMember && relY < h * 0.2) return `alt-tl-group:${gId}`;
          if (!draggedIsGroupMember && relY > h * 0.8) return `alt-tl-group-after:${gId}`;

          const candidates = item.blocks.filter(b => b.id !== draggingItemId);
          if (candidates.length === 0) return `alt-group-drop:${gId}`;

          let nearest = candidates[0];
          let nearestMid = midOf(nearest.id) ?? (y + h / 2);
          for (const c of candidates.slice(1)) {
            const mid = midOf(c.id) ?? nearestMid;
            if (Math.abs(cursorY - mid) < Math.abs(cursorY - nearestMid)) {
              nearest = c;
              nearestMid = mid;
            }
          }
          return cursorY >= nearestMid ? `alt-after:${nearest.id}` : nearest.id;
        }

        const altId = item.row.id;
        if (groupDrag) {
          return cursorY >= y + h / 2 ? `alt-row-after:${altId}` : `alt-row:${altId}`;
        }
        const midY = midOf(altId) ?? (y + h / 2);
        return cursorY >= midY ? `alt-after:${altId}` : altId;
      }
    }

    // Below every item: after the last one, in this detector's own ids.
    const last = [...items].reverse().find(item => altTlId(item) !== dragTlId);
    if (!last) return null;
    if (last.kind === "alt-group") {
      return groupDrag ? `alt-group-after:${last.id}` : `alt-tl-group-after:${last.id}`;
    }
    return groupDrag ? `alt-row-after:${last.row.id}` : `alt-after:${last.row.id}`;
  };

  const handleAltDragStart = (id: string, kind: "row" | "group", event: PointerEvent | MouseEvent) => {
    engine.startDrag(id, kind, event);
  };

  const previewAltTLItems = createMemo((): AltTLItem[] => {
    const items = altTopLevelItems();
    const dragging = engine.draggingId();
    const drop = engine.hoveredDropId();
    if (!dragging || !drop) return items;

    if (isGroupDrag()) {
      const draggingItem = items.find(i => i.kind === "alt-group" && i.id === dragging);
      if (!draggingItem) return items;

      const rest = items.filter(i => !(i.kind === "alt-group" && i.id === dragging));
      let insertIdx: number;

      if (drop.startsWith("alt-group-after:")) {
        const gId = drop.slice("alt-group-after:".length);
        const idx = rest.findIndex(i => i.kind === "alt-group" && i.id === gId);
        insertIdx = idx < 0 ? rest.length : idx + 1;
      } else if (drop.startsWith("alt-group:")) {
        const gId = drop.slice("alt-group:".length);
        const idx = rest.findIndex(i => i.kind === "alt-group" && i.id === gId);
        insertIdx = idx < 0 ? rest.length : idx;
      } else if (drop.startsWith("alt-row-after:")) {
        const rowId = drop.slice("alt-row-after:".length);
        const idx = rest.findIndex(i => i.kind === "alt-row" && i.row.id === rowId);
        insertIdx = idx < 0 ? rest.length : idx + 1;
      } else if (drop.startsWith("alt-row:")) {
        const rowId = drop.slice("alt-row:".length);
        const idx = rest.findIndex(i => i.kind === "alt-row" && i.row.id === rowId);
        insertIdx = idx < 0 ? rest.length : idx;
      } else {
        return items;
      }

      const result = [...rest];
      result.splice(insertIdx, 0, draggingItem);
      return result;
    }

    if (drop.startsWith("alt-group-drop:")) return items;

    const draggingIdx = items.findIndex(i => i.kind === "alt-row" && i.row.id === dragging);
    if (draggingIdx < 0) return items;

    const draggingItem = items[draggingIdx];
    const rest = items.filter((_, idx) => idx !== draggingIdx);
    let insertIdx: number;

    if (drop.startsWith("alt-tl-group:")) {
      const gId = drop.slice("alt-tl-group:".length);
      const idx = rest.findIndex(i => i.kind === "alt-group" && i.id === gId);
      insertIdx = idx < 0 ? rest.length : idx;
    } else if (drop.startsWith("alt-tl-group-after:")) {
      const gId = drop.slice("alt-tl-group-after:".length);
      const idx = rest.findIndex(i => i.kind === "alt-group" && i.id === gId);
      insertIdx = idx < 0 ? rest.length : idx + 1;
    } else if (drop.startsWith("alt-after:")) {
      const afterId = drop.slice("alt-after:".length);
      const idx = rest.findIndex(i => i.kind === "alt-row" && i.row.id === afterId);
      if (idx < 0) return items;
      insertIdx = idx + 1;
    } else {
      const idx = rest.findIndex(i => i.kind === "alt-row" && i.row.id === drop);
      if (idx < 0) return items;
      insertIdx = idx;
    }

    const result = [...rest];
    result.splice(insertIdx, 0, draggingItem);
    return result;
  });

  const previewAltTLTranslates = createMemo((): Map<string, number> => {
    if (!engine.anyDragging()) return new Map();
    if (engine.hoveredDropId()?.startsWith("alt-group-drop:")) return new Map();
    if (!engine.draggingId() || !engine.hoveredDropId()) return new Map();
    const items = altTopLevelItems();
    const preview = previewAltTLItems();
    return computePreviewTranslates(
      items.map(altTlId),
      preview.map(altTlId),
      engine.cachedHeights(),
      0
    );
  });

  const previewGroupedTranslates = createMemo((): Map<string, number> => {
    const map = new Map<string, number>();
    const dragging = engine.draggingId();
    const drop = engine.hoveredDropId();
    if (!dragging || !drop || isGroupDrag()) return map;

    for (const group of scopedAltGroups()) {
      const ids = group.blocks.map(a => a.id);
      if (!ids.includes(dragging)) continue;

      let insertIdx: number;
      if (drop === `alt-group-drop:${group.id}`) {
        insertIdx = ids.length;
      } else if (drop.startsWith("alt-after:")) {
        const afterId = drop.slice("alt-after:".length);
        const ai = ids.indexOf(afterId);
        if (ai < 0) continue;
        insertIdx = ai + 1;
      } else if (ids.includes(drop)) {
        insertIdx = ids.indexOf(drop);
      } else {
        continue;
      }

      const fromIdx = ids.indexOf(dragging);
      const next = [...ids];
      next.splice(fromIdx, 1);
      next.splice(insertIdx > fromIdx ? insertIdx - 1 : insertIdx, 0, dragging);

      const translates = computePreviewTranslates(ids, next, engine.cachedHeights(), 0);
      for (const [id, offset] of translates) {
        map.set(id, offset);
      }
    }
    return map;
  });

  const commitAltDrop = (fromId: string, dropId: string) => {
    const parentId = props.parentRow.id;
    const rawScoped = aestheticGroups().filter(g => g.scopeRowId === parentId);
    const membership = altGroupMembership();
    const groups = rawScoped.map(g => ({ ...g, blockIds: [...g.blockIds] }));
    let ungroupedIds = (props.parentRow.alternatives ?? [])
      .map(a => a.id)
      .filter(id => !membership.has(id));

    const srcGroupId = membership.get(fromId) ?? null;
    if (srcGroupId) {
      const src = groups.find(g => g.id === srcGroupId);
      if (src) src.blockIds = removeFrom(src.blockIds, fromId);
    } else {
      ungroupedIds = removeFrom(ungroupedIds, fromId);
    }

    let useTLOrdering = false;
    let tlInsertTarget: { kind: "before-group"; gId: string } | { kind: "after-group"; gId: string } | null = null;

    if (dropId.startsWith("alt-group-drop:")) {
      const tgtId = dropId.replace("alt-group-drop:", "");
      const tgt = groups.find(g => g.id === tgtId);
      if (tgt) tgt.blockIds = [...removeFrom(tgt.blockIds, fromId), fromId];
    } else if (dropId.startsWith("alt-tl-group:")) {
      const tgId = dropId.slice("alt-tl-group:".length);
      ungroupedIds = [...ungroupedIds, fromId];
      useTLOrdering = true;
      tlInsertTarget = { kind: "before-group", gId: tgId };
    } else if (dropId.startsWith("alt-tl-group-after:")) {
      const tgId = dropId.slice("alt-tl-group-after:".length);
      ungroupedIds = [...ungroupedIds, fromId];
      useTLOrdering = true;
      tlInsertTarget = { kind: "after-group", gId: tgId };
    } else if (dropId === "alt-ungrouped-drop") {
      ungroupedIds = [...ungroupedIds, fromId];
    } else if (dropId.startsWith("alt-after:")) {
      const afterId = dropId.replace("alt-after:", "");
      const afterGroupId = membership.get(afterId) ?? null;
      if (afterGroupId) {
        const tgt = groups.find(g => g.id === afterGroupId);
        if (tgt) {
          const ai = tgt.blockIds.indexOf(afterId);
          if (ai < 0 || ai === tgt.blockIds.length - 1) {
            tgt.blockIds = [...removeFrom(tgt.blockIds, fromId), fromId];
          } else {
            tgt.blockIds = insertBefore(tgt.blockIds, fromId, tgt.blockIds[ai + 1]);
          }
        }
      } else {
        const ai = ungroupedIds.indexOf(afterId);
        if (ai < 0 || ai === ungroupedIds.length - 1) {
          ungroupedIds = [...ungroupedIds, fromId];
        } else {
          ungroupedIds = insertBefore(ungroupedIds, fromId, ungroupedIds[ai + 1]);
        }
      }
    } else {
      const tgtGroupId = membership.get(dropId) ?? null;
      if (tgtGroupId) {
        const tgt = groups.find(g => g.id === tgtGroupId);
        if (tgt) tgt.blockIds = insertBefore(tgt.blockIds, fromId, dropId);
      } else {
        ungroupedIds = insertBefore(ungroupedIds, fromId, dropId);
      }
    }

    const interleaved: string[] = [];
    const seenGroups = new Set<string>();
    const ungroupedSet = new Set(ungroupedIds);
    let ungroupedPos = 0;
    for (const alt of (props.parentRow.alternatives ?? [])) {
      if (alt.id === fromId && !membership.has(fromId) && !ungroupedSet.has(fromId)) continue;
      const gId = membership.get(alt.id);
      if (gId) {
        if (!seenGroups.has(gId)) {
          seenGroups.add(gId);
          const g = groups.find(gg => gg.id === gId);
          if (g) interleaved.push(...g.blockIds);
        }
      } else if (ungroupedSet.has(alt.id)) {
        interleaved.push(ungroupedIds[ungroupedPos++]);
      }
    }
    for (const g of groups) {
      if (!seenGroups.has(g.id) && g.blockIds.length > 0) interleaved.push(...g.blockIds);
    }
    for (const id of ungroupedIds) {
      if (!interleaved.includes(id)) interleaved.push(id);
    }

    let newOrderedIds: string[];
    if (useTLOrdering && tlInsertTarget) {
      const withoutDragged = interleaved.filter(id => id !== fromId);
      let insertIdx: number;
      const tg = groups.find(g => g.id === tlInsertTarget!.gId);
      if (tlInsertTarget.kind === "before-group") {
        const firstMember = tg?.blockIds[0];
        insertIdx = firstMember ? withoutDragged.indexOf(firstMember) : withoutDragged.length;
        if (insertIdx < 0) insertIdx = withoutDragged.length;
      } else {
        const lastMember = tg?.blockIds[tg.blockIds.length - 1];
        insertIdx = lastMember ? withoutDragged.indexOf(lastMember) + 1 : withoutDragged.length;
        if (insertIdx < 0) insertIdx = withoutDragged.length;
      }
      withoutDragged.splice(insertIdx, 0, fromId);
      newOrderedIds = withoutDragged;
    } else {
      newOrderedIds = interleaved;
    }

    const curRows = modRowsState();
    const parentRow = findRowByIdDeep(curRows, parentId);
    const altMap = new Map((parentRow?.alternatives ?? []).map(a => [a.id, a]));
    const newAlts = newOrderedIds.map(id => altMap.get(id)).filter((a): a is ModRow => Boolean(a));
    const newRows = updateAltsDeep(curRows, parentId, newAlts);

    const filteredGroups: AestheticGroup[] = groups
      .filter(g => g.blockIds.length > 0)
      .map(g => ({
        id: g.id,
        name: g.name,
        collapsed: g.collapsed,
        blockIds: g.blockIds,
        scopeRowId: g.scopeRowId,
      }));
    const newAestheticGroups = [
      ...aestheticGroups().filter(g => g.scopeRowId !== parentId),
      ...filteredGroups,
    ];

    batch(() => {
      setAestheticGroups(newAestheticGroups);
      setModRowsState(newRows);
    });
    props.onReorderAlts?.(parentId, newOrderedIds);
  };

  const commitAltGroupDrop = (fromGroupId: string, dropId: string) => {
    const parentId = props.parentRow.id;
    const scoped = aestheticGroups().filter(g => g.scopeRowId === parentId);
    const others = aestheticGroups().filter(g => g.scopeRowId !== parentId);
    const dragging = scoped.find(g => g.id === fromGroupId);
    if (!dragging) return;

    const rest = scoped.filter(g => g.id !== fromGroupId);
    let insertIdx: number;

    if (dropId.startsWith("alt-group-after:")) {
      const tgId = dropId.slice("alt-group-after:".length);
      const idx = rest.findIndex(g => g.id === tgId);
      insertIdx = idx < 0 ? rest.length : idx + 1;
    } else if (dropId.startsWith("alt-group:")) {
      const tgId = dropId.slice("alt-group:".length);
      const idx = rest.findIndex(g => g.id === tgId);
      insertIdx = idx < 0 ? rest.length : idx;
    } else {
      insertIdx = rest.length;
    }

    const reordered = [...rest];
    reordered.splice(insertIdx, 0, dragging);

    const groupMemberIds = new Set(dragging.blockIds);
    const curRows = modRowsState();
    const parentRow = findRowByIdDeep(curRows, parentId);
    if (!parentRow) return;

    const curAlts = parentRow.alternatives ?? [];
    const groupMembers = dragging.blockIds
      .map(id => curAlts.find(a => a.id === id))
      .filter((a): a is ModRow => Boolean(a));
    const otherAlts = curAlts.filter(a => !groupMemberIds.has(a.id));

    let altInsertIdx: number;
    if (dropId.startsWith("alt-group-after:")) {
      const tgId = dropId.slice("alt-group-after:".length);
      const tg = scoped.find(g => g.id === tgId);
      let last = -1;
      if (tg) {
        for (let i = otherAlts.length - 1; i >= 0; i--) {
          if (tg.blockIds.includes(otherAlts[i].id)) {
            last = i;
            break;
          }
        }
      }
      altInsertIdx = last < 0 ? otherAlts.length : last + 1;
    } else if (dropId.startsWith("alt-group:")) {
      const tgId = dropId.slice("alt-group:".length);
      const tg = scoped.find(g => g.id === tgId);
      const first = tg?.blockIds[0];
      altInsertIdx = first ? otherAlts.findIndex(a => a.id === first) : otherAlts.length;
      if (altInsertIdx < 0) altInsertIdx = otherAlts.length;
    } else if (dropId.startsWith("alt-row-after:")) {
      const rowId = dropId.slice("alt-row-after:".length);
      const idx = otherAlts.findIndex(a => a.id === rowId);
      altInsertIdx = idx < 0 ? otherAlts.length : idx + 1;
    } else if (dropId.startsWith("alt-row:")) {
      const rowId = dropId.slice("alt-row:".length);
      const idx = otherAlts.findIndex(a => a.id === rowId);
      altInsertIdx = idx < 0 ? otherAlts.length : idx;
    } else {
      altInsertIdx = otherAlts.length;
    }

    const newAlts = [...otherAlts];
    newAlts.splice(altInsertIdx, 0, ...groupMembers);
    const newRows = updateAltsDeep(curRows, parentId, newAlts);

    batch(() => {
      setAestheticGroups([...others, ...reordered]);
      setModRowsState(newRows);
    });
    props.onReorderAlts?.(parentId, newAlts.map(a => a.id));
  };

  const handleNestedReorderAlts = (parentId: string, orderedIds: string[]) => {
    setModRowsState(cur => {
      const parentRow = findRowByIdDeep(cur, parentId);
      if (!parentRow) return cur;
      const altMap = new Map((parentRow.alternatives ?? []).map(x => [x.id, x]));
      const newAlts = orderedIds.map(xid => altMap.get(xid)).filter((x): x is ModRow => Boolean(x));
      return updateAltsDeep(cur, parentId, newAlts);
    });
    props.onReorderAlts?.(parentId, orderedIds);
  };

  return {
    altTopLevelItems,
    draggingAlt,
    draggingAltGroupData,
    editingGroupId,
    engine,
    groupNameDraft,
    handleAltDragStart,
    handleNestedReorderAlts,
    isGroupDrag,
    modIcons,
    previewAltTLTranslates,
    previewGroupedTranslates,
    removeAestheticGroup,
    scopedAltGroups,
    setContainerRef,
    setGroupNameDraft,
    startGroupRename,
    commitGroupRename,
    toggleGroupCollapsed,
  };
}
