import { batch, createEffect, createMemo, onCleanup } from "solid-js";
import { appendDebugTrace } from "../../lib/debugTrace";
import { useDragEngine, type DragItem } from "../../lib/dragEngine";
import { computePreviewTranslates } from "../../lib/dragUtils";
import type { ModRow } from "../../lib/types";
import {
  activeAccount,
  aestheticGroups,
  modIcons,
  modRowsState,
  setAestheticGroups,
  setModRowsState,
  topLevelItems,
  type TopLevelItem,
} from "../../store";

export interface ModListEditorDragOptions {
  onReorder: (orderedIds: string[]) => void;
}

export const tlId = (item: TopLevelItem) => item.kind === "row" ? item.row.id : `group:${item.id}`;

export function useModListEditorDrag(options: ModListEditorDragOptions) {
  let listContainerRef: HTMLDivElement | undefined;
  let scrollContainerRef: HTMLDivElement | undefined;

  const setListContainerRef = (element: HTMLDivElement | undefined) => {
    listContainerRef = element;
  };

  const setScrollContainerRef = (element: HTMLDivElement | undefined) => {
    scrollContainerRef = element;
  };

  const engine = useDragEngine({
    containerRef: () => listContainerRef,
    getItems: () => topLevelItems().map((item): DragItem =>
      item.kind === "row"
        ? { kind: "row", id: item.row.id }
        : { kind: "group", id: item.id }
    ),
    onCommit: (fromId, dropId, fromKind) => commitDrop(fromId, dropId, fromKind),
    detect: cursorY => detectEnhancedDropTarget(cursorY),
  });

  {
    const EDGE_PX = 50;
    const SPEED = 6;
    let autoScrollFrame = 0;

    const tick = () => {
      const pointer = engine.dragPointer();
      const element = scrollContainerRef;
      if (!pointer || !element || !engine.anyDragging()) {
        autoScrollFrame = 0;
        return;
      }
      const rect = element.getBoundingClientRect();
      const distTop = pointer.y - rect.top;
      const distBottom = rect.bottom - pointer.y;
      if (distTop < EDGE_PX) {
        element.scrollTop -= SPEED * (1 - distTop / EDGE_PX);
      } else if (distBottom < EDGE_PX) {
        element.scrollTop += SPEED * (1 - distBottom / EDGE_PX);
      }
      autoScrollFrame = requestAnimationFrame(tick);
    };

    createEffect(() => {
      if (engine.anyDragging()) {
        autoScrollFrame = requestAnimationFrame(tick);
      } else if (autoScrollFrame) {
        cancelAnimationFrame(autoScrollFrame);
        autoScrollFrame = 0;
      }
    });

    onCleanup(() => {
      if (autoScrollFrame) cancelAnimationFrame(autoScrollFrame);
    });
  }

  const isDraggingGroup = () => engine.draggingKind() === "group";

  const draggingRow = () => {
    const id = engine.draggingId();
    if (!id || engine.draggingKind() === "group") return null;
    return modRowsState().find(row => row.id === id) ?? null;
  };

  const draggingGroupItem = () => {
    const id = engine.draggingId();
    if (!id || engine.draggingKind() !== "group") return null;
    const found = topLevelItems().find(item => item.kind === "group" && item.id === id);
    return found?.kind === "group" ? found : null;
  };

  const draggingRowIcon = () => {
    const row = draggingRow();
    return row?.modrinth_id ? modIcons().get(row.modrinth_id) : undefined;
  };

  const groupByRowId = () => {
    const map = new Map<string, string>();
    for (const group of aestheticGroups()) {
      for (const id of group.blockIds) map.set(id, group.id);
    }
    return map;
  };

  const detectEnhancedDropTarget = (cursorY: number): string | null => {
    const items = topLevelItems();
    const heights = engine.cachedHeights();
    const tops = engine.cachedTops();
    const dragId = engine.draggingId()!;
    const groupDrag = isDraggingGroup();
    const dragTlId = groupDrag ? `group:${dragId}` : dragId;
    const scrollDelta = engine.scrollDelta();

    for (const item of items) {
      const id = tlId(item);
      if (id === dragTlId) continue;

      const rawY = tops.get(id);
      const height = heights.get(id) ?? 40;
      if (rawY === undefined) continue;
      const y = rawY - scrollDelta;

      if (cursorY < y + height) {
        if (item.kind === "group") {
          const groupId = item.id;
          const relY = cursorY - y;

          if (groupDrag) {
            return cursorY >= y + height / 2 ? `tl-group-after:${groupId}` : `tl-group:${groupId}`;
          }

          const draggedIsGroupMember = groupByRowId().get(dragId) === groupId;
          const edgePx = Math.min(20, height * 0.15);
          if (!draggedIsGroupMember && relY < edgePx) return `tl-group:${groupId}`;
          if (!draggedIsGroupMember && relY > height - edgePx) return `tl-group-after:${groupId}`;

          const midYs = engine.cachedMidYs();
          const candidates = item.blocks.filter(block => block.id !== dragTlId);
          if (candidates.length === 0) return `group-drop:${groupId}`;
          let nearest = candidates[0];
          for (const candidate of candidates.slice(1)) {
            const candidateMid = (midYs.get(candidate.id) ?? Infinity) - scrollDelta;
            const nearestMid = (midYs.get(nearest.id) ?? Infinity) - scrollDelta;
            if (Math.abs(cursorY - candidateMid) < Math.abs(cursorY - nearestMid)) nearest = candidate;
          }
          const nearestMid = (midYs.get(nearest.id) ?? (rawY + height / 2)) - scrollDelta;
          return cursorY >= nearestMid ? `row-after:${nearest.id}` : nearest.id;
        }

        const rowId = item.row.id;
        return cursorY >= y + height / 2 ? `row-after:${rowId}` : rowId;
      }
    }

    // Below every item: after the last one, in this detector's own ids.
    const last = [...items].reverse().find(item => tlId(item) !== dragTlId);
    if (!last) return null;
    return last.kind === "group" ? `tl-group-after:${last.id}` : `row-after:${last.row.id}`;
  };

  const handleStartDrag = (id: string, kind: "row" | "group", event: PointerEvent | MouseEvent) => {
    engine.startDrag(id, kind, event);
  };

  const previewTLItems = createMemo(() => {
    const items = topLevelItems();
    const dragging = engine.draggingId();
    const drop = engine.hoveredDropId();
    if (!dragging || !drop) return items;
    if (drop.startsWith("group-drop:")) return items;

    const dragTlId = isDraggingGroup() ? `group:${dragging}` : dragging;
    const draggingItem = items.find(item => tlId(item) === dragTlId);
    if (!draggingItem) return items;

    const rest = items.filter(item => tlId(item) !== dragTlId);
    let insertIdx: number;

    if (drop.startsWith("tl-group-after:")) {
      const groupId = drop.slice("tl-group-after:".length);
      const idx = rest.findIndex(item => item.kind === "group" && item.id === groupId);
      insertIdx = idx < 0 ? rest.length : idx + 1;
    } else if (drop.startsWith("tl-group:")) {
      const groupId = drop.slice("tl-group:".length);
      const idx = rest.findIndex(item => item.kind === "group" && item.id === groupId);
      insertIdx = idx < 0 ? rest.length : idx;
    } else if (drop.startsWith("row-after:")) {
      const rowId = drop.slice("row-after:".length);
      const idx = rest.findIndex(item => item.kind === "row" && item.row.id === rowId);
      insertIdx = idx < 0 ? rest.length : idx + 1;
    } else {
      const idx = rest.findIndex(item => item.kind === "row" && item.row.id === drop);
      insertIdx = idx < 0 ? rest.length : idx;
    }

    const result = [...rest];
    result.splice(insertIdx, 0, draggingItem);
    return result;
  });

  const previewTranslates = createMemo(() => {
    if (!engine.draggingId() || !engine.hoveredDropId() || engine.hoveredDropId()?.startsWith("group-drop:")) {
      return new Map<string, number>();
    }
    return computePreviewTranslates(
      topLevelItems().map(tlId),
      previewTLItems().map(tlId),
      engine.cachedHeights(),
      0
    );
  });

  const previewGroupRowTranslates = createMemo(() => {
    const map = new Map<string, number>();
    const dragging = engine.draggingId();
    const drop = engine.hoveredDropId();
    if (!dragging || !drop) return map;

    for (const item of topLevelItems()) {
      if (item.kind !== "group") continue;
      const ids = item.blocks.map(row => row.id);
      if (!ids.includes(dragging)) continue;

      let insertIdx: number;
      if (drop.startsWith("row-after:")) {
        const afterIdx = ids.indexOf(drop.slice("row-after:".length));
        if (afterIdx < 0) continue;
        insertIdx = afterIdx + 1;
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
      for (const [id, offset] of translates) map.set(id, offset);
    }
    return map;
  });

  const removeFromArr = (ids: string[], id: string) => ids.filter(candidate => candidate !== id);

  const commitGroupMove = (groupId: string, dropId: string) => {
    const group = aestheticGroups().find(candidate => candidate.id === groupId);
    if (!group) return;

    const groupRowIds = new Set(group.blockIds);
    const groupRows = group.blockIds
      .map(id => modRowsState().find(row => row.id === id))
      .filter((row): row is ModRow => Boolean(row));
    const otherRows = modRowsState().filter(row => !groupRowIds.has(row.id));

    let insertIdx: number;
    if (dropId.startsWith("tl-group:")) {
      const targetGroupId = dropId.slice("tl-group:".length);
      const targetGroup = aestheticGroups().find(candidate => candidate.id === targetGroupId);
      const first = targetGroup?.blockIds[0];
      insertIdx = first ? otherRows.findIndex(row => row.id === first) : otherRows.length;
      if (insertIdx < 0) insertIdx = otherRows.length;
    } else if (dropId.startsWith("tl-group-after:")) {
      const targetGroupId = dropId.slice("tl-group-after:".length);
      const targetGroup = aestheticGroups().find(candidate => candidate.id === targetGroupId);
      let last = -1;
      if (targetGroup) {
        for (let i = otherRows.length - 1; i >= 0; i--) {
          if (targetGroup.blockIds.includes(otherRows[i].id)) {
            last = i;
            break;
          }
        }
      }
      insertIdx = last < 0 ? otherRows.length : last + 1;
    } else if (dropId.startsWith("row-after:")) {
      const rowId = dropId.slice("row-after:".length);
      const idx = otherRows.findIndex(row => row.id === rowId);
      insertIdx = idx < 0 ? otherRows.length : idx + 1;
    } else {
      const idx = otherRows.findIndex(row => row.id === dropId);
      insertIdx = idx < 0 ? otherRows.length : idx;
    }

    const nextRows = [...otherRows];
    nextRows.splice(insertIdx, 0, ...groupRows);
    batch(() => {
      setModRowsState(nextRows);
    });
    options.onReorder(nextRows.map(row => row.id));
  };

  const commitRowMove = (fromId: string, dropId: string) => {
    const groups = aestheticGroups().map(group => ({ ...group, blockIds: [...group.blockIds] }));
    const membership = groupByRowId();
    const sourceGroupId = membership.get(fromId) ?? null;

    if (sourceGroupId) {
      const sourceGroup = groups.find(group => group.id === sourceGroupId);
      if (sourceGroup) sourceGroup.blockIds = removeFromArr(sourceGroup.blockIds, fromId);
    }

    if (dropId.startsWith("group-drop:")) {
      const targetGroupId = dropId.slice("group-drop:".length);
      const targetGroup = groups.find(group => group.id === targetGroupId);
      if (targetGroup && !targetGroup.blockIds.includes(fromId)) targetGroup.blockIds.push(fromId);

      const row = modRowsState().find(candidate => candidate.id === fromId);
      if (!row) return;
      const others = modRowsState().filter(candidate => candidate.id !== fromId);
      let lastIdx = -1;
      if (targetGroup) {
        for (let i = others.length - 1; i >= 0; i--) {
          if (targetGroup.blockIds.includes(others[i].id)) {
            lastIdx = i;
            break;
          }
        }
      }
      const insertIdx = lastIdx < 0 ? others.length : lastIdx + 1;
      const nextRows = [...others];
      nextRows.splice(insertIdx, 0, row);
      batch(() => {
        setAestheticGroups(groups.filter(group => group.blockIds.length > 0));
        setModRowsState(nextRows);
      });
      options.onReorder(nextRows.map(candidate => candidate.id));
      return;
    }

    let targetRowId: string | null = null;
    if (dropId.startsWith("row-after:")) {
      targetRowId = dropId.slice("row-after:".length);
    } else if (!dropId.startsWith("tl-group:") && !dropId.startsWith("tl-group-after:")) {
      targetRowId = dropId;
    }
    if (targetRowId) {
      const targetGroupId = membership.get(targetRowId);
      if (targetGroupId) {
        const targetGroup = groups.find(group => group.id === targetGroupId);
        if (targetGroup && !targetGroup.blockIds.includes(fromId)) {
          const targetIdx = targetGroup.blockIds.indexOf(targetRowId);
          if (dropId.startsWith("row-after:")) {
            targetGroup.blockIds.splice(targetIdx < 0 ? targetGroup.blockIds.length : targetIdx + 1, 0, fromId);
          } else {
            targetGroup.blockIds.splice(targetIdx < 0 ? 0 : targetIdx, 0, fromId);
          }
        }
      }
    }

    const row = modRowsState().find(candidate => candidate.id === fromId);
    if (!row) return;
    const others = modRowsState().filter(candidate => candidate.id !== fromId);
    const filteredGroups = groups.filter(group => group.blockIds.length > 0);

    let insertIdx: number;
    if (dropId.startsWith("tl-group:")) {
      const targetGroupId = dropId.slice("tl-group:".length);
      const targetGroup = filteredGroups.find(group => group.id === targetGroupId);
      const first = targetGroup?.blockIds[0];
      insertIdx = first ? others.findIndex(candidate => candidate.id === first) : others.length;
      if (insertIdx < 0) insertIdx = others.length;
    } else if (dropId.startsWith("tl-group-after:")) {
      const targetGroupId = dropId.slice("tl-group-after:".length);
      const targetGroup = filteredGroups.find(group => group.id === targetGroupId);
      let last = -1;
      if (targetGroup) {
        for (let i = others.length - 1; i >= 0; i--) {
          if (targetGroup.blockIds.includes(others[i].id)) {
            last = i;
            break;
          }
        }
      }
      insertIdx = last < 0 ? others.length : last + 1;
    } else if (dropId.startsWith("row-after:")) {
      const rowId = dropId.slice("row-after:".length);
      const idx = others.findIndex(candidate => candidate.id === rowId);
      insertIdx = idx < 0 ? others.length : idx + 1;
    } else {
      const idx = others.findIndex(candidate => candidate.id === dropId);
      insertIdx = idx < 0 ? others.length : idx;
    }

    const nextRows = [...others];
    nextRows.splice(insertIdx, 0, row);
    batch(() => {
      setAestheticGroups(filteredGroups);
      setModRowsState(nextRows);
    });
    options.onReorder(nextRows.map(candidate => candidate.id));
  };

  const commitDrop = (fromId: string, dropId: string, fromKind: "row" | "group") => {
    if (fromId === dropId) return;
    appendDebugTrace("groups.drag.frontend", { phase: "drop", draggableId: fromId, droppableId: dropId });
    if (fromKind === "group") commitGroupMove(fromId, dropId);
    else commitRowMove(fromId, dropId);
  };

  const isSignedIn = () => activeAccount()?.status === "online";

  return {
    draggingGroupItem,
    draggingRow,
    draggingRowIcon,
    engine,
    handleStartDrag,
    isDraggingGroup,
    isSignedIn,
    previewGroupRowTranslates,
    previewTranslates,
    setListContainerRef,
    setScrollContainerRef,
  };
}
