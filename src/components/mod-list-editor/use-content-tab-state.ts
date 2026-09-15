import { invoke } from "@tauri-apps/api/core";
import { batch, createEffect, createMemo, createSignal } from "solid-js";
import { selectedMcVersion, selectedModLoader } from "../../store";
import { useDragEngine, type DragItem } from "../../lib/dragEngine";
import { computePreviewTranslates } from "../../lib/dragUtils";
import type {
  ContentEntry,
  ContentGroupData,
  ContentMeta,
  ContentSnapshotPayload,
  ContentTabViewProps,
  ContentTopLevelItem,
} from "./content-types";
import { CONTENT_TAB_LABELS } from "./content-types";

const [contentVersion, setContentVersion] = createSignal(0);

export function bumpContentVersion() {
  setContentVersion(v => v + 1);
}

// Readable titles for Modrinth content ids/slugs. Filled from the add dialog
// (seedContentName) and from successful metadata fetches so cards keep showing
// titles instead of raw project IDs when a later fetch is unavailable.
const contentNameCache = new Map<string, string>();

export function seedContentName(id: string, name: string) {
  if (id && name && name !== id) contentNameCache.set(id, name);
}

const ctlId = (item: ContentTopLevelItem) => item.kind === "entry" ? item.entry.id : `group:${item.id}`;

const removeFromArr = (arr: string[], id: string) => arr.filter(x => x !== id);

const mcVersionMatches = (pattern: string, concrete: string): boolean => {
  if (pattern === concrete) return true;
  const lower = pattern.toLowerCase();
  if (!lower.endsWith(".x")) return false;
  const prefix = pattern.slice(0, -2);
  return concrete.startsWith(prefix) && concrete[prefix.length] === ".";
};

export function useContentTabState(props: ContentTabViewProps) {
  let listContainerRef: HTMLDivElement | undefined;

  const [entries, setEntries] = createSignal<ContentEntry[]>([]);
  const [groups, setGroups] = createSignal<ContentGroupData[]>([]);
  const [meta, setMeta] = createSignal<Map<string, ContentMeta>>(new Map());
  const [selectedIds, setSelectedIds] = createSignal<Set<string>>(new Set());
  const [editingGroupId, setEditingGroupId] = createSignal<string | null>(null);
  const [groupNameDraft, setGroupNameDraft] = createSignal("");
  const [advancedEntryId, setAdvancedEntryId] = createSignal<string | null>(null);

  const setListContainerRef = (element: HTMLDivElement | undefined) => {
    listContainerRef = element;
  };

  const label = () => CONTENT_TAB_LABELS[props.type] ?? props.type;

  const load = async () => {
    if (!props.modlistName) return;
    try {
      const snap = await invoke<ContentSnapshotPayload>("load_content_list_command", {
        input: { modlistName: props.modlistName, contentType: props.type },
      });
      const list: ContentEntry[] = (snap.entries ?? []).map(entry => ({
        id: entry.id,
        source: entry.source,
        versionRules: (entry.versionRules ?? []).map(rule => ({
          kind: rule.kind,
          mcVersions: rule.mcVersions,
          loader: rule.loader,
        })),
        name: entry.name ?? undefined,
        iconImage: entry.iconImage ?? undefined,
      }));
      const nextGroups: ContentGroupData[] = snap.groups ?? [];
      setEntries(list);
      setGroups(nextGroups);

      const nextMeta = new Map<string, ContentMeta>();
      // A local pack answers for itself: the snapshot already carries the name
      // and the icon, so nothing here asks Modrinth about it. A pack with no
      // pack.png simply has no icon, which is the normal case and not an error
      // — the row falls back to the generic icon.
      for (const entry of list) {
        if (entry.source !== "local") continue;
        nextMeta.set(entry.id, { name: entry.name ?? entry.id, iconUrl: entry.iconImage });
      }

      const modrinthIds = list.filter(entry => entry.source === "modrinth").map(entry => entry.id);
      // Show already-known readable names immediately; the fetch below only
      // refines them (icons, corrected titles). If it fails, cached names keep
      // cards from falling back to raw project IDs.
      for (const id of modrinthIds) {
        const cached = contentNameCache.get(id);
        if (cached) nextMeta.set(id, { name: cached });
      }
      setMeta(nextMeta);
      // Only the Modrinth half needs the API; a list of local packs only is
      // already complete here.
      if (modrinthIds.length === 0) return;

      try {
        const param = encodeURIComponent(JSON.stringify(modrinthIds));
        const response = await fetch(`https://api.modrinth.com/v2/projects?ids=${param}`, {
          headers: { "User-Agent": "CubicLauncher/0.1.0" },
        });
        if (!response.ok) return;
        const projects: Array<{ id: string; slug: string; title: string; icon_url?: string | null }> = await response.json();
        for (const project of projects) {
          const data = { name: project.title, iconUrl: project.icon_url ?? undefined };
          if (project.slug) nextMeta.set(project.slug, data);
          if (project.id) nextMeta.set(project.id, data);
          if (project.title) {
            if (project.slug) contentNameCache.set(project.slug, project.title);
            if (project.id) contentNameCache.set(project.id, project.title);
          }
        }
        setMeta(new Map(nextMeta));
      } catch {
        // best effort
      }
    } catch {
      setEntries([]);
      setGroups([]);
    }
  };

  createEffect(() => {
    contentVersion();
    if (props.modlistName) void load();
  });

  const groupByEntryId = () => {
    const map = new Map<string, string>();
    for (const group of groups()) {
      for (const id of group.entryIds) map.set(id, group.id);
    }
    return map;
  };

  const contentTLItems = createMemo((): ContentTopLevelItem[] => {
    const currentGroups = groups();
    const allEntries = entries();
    const entryMap = new Map(allEntries.map(entry => [entry.id, entry]));
    const items: ContentTopLevelItem[] = [];
    const emittedGroups = new Set<string>();

    for (const entry of allEntries) {
      const groupId = groupByEntryId().get(entry.id);
      if (!groupId) {
        items.push({ kind: "entry", entry });
        continue;
      }
      if (emittedGroups.has(groupId)) continue;
      emittedGroups.add(groupId);
      const group = currentGroups.find(candidate => candidate.id === groupId);
      if (!group) continue;
      items.push({
        kind: "group",
        id: group.id,
        name: group.name,
        collapsed: group.collapsed,
        entries: group.entryIds
          .map(entryId => entryMap.get(entryId))
          .filter((groupEntry): groupEntry is ContentEntry => Boolean(groupEntry)),
      });
    }

    return items;
  });

  const saveGroups = async (nextGroups: ContentGroupData[]) => {
    try {
      await invoke("save_content_groups_command", {
        input: { modlistName: props.modlistName, contentType: props.type, groups: nextGroups },
      });
    } catch {
      // ignore persistence failures for optimistic UI
    }
  };

  const saveOrder = async (ordered: ContentEntry[]) => {
    try {
      await invoke("reorder_content_command", {
        input: { modlistName: props.modlistName, contentType: props.type, orderedIds: ordered.map(entry => entry.id) },
      });
    } catch {
      // ignore persistence failures for optimistic UI
    }
  };

  const removeEntry = async (id: string) => {
    try {
      await invoke("remove_content_command", {
        input: { modlistName: props.modlistName, contentType: props.type, id },
      });
      setEntries(current => current.filter(entry => entry.id !== id));
      setGroups(current => current
        .map(group => ({ ...group, entryIds: group.entryIds.filter(entryId => entryId !== id) }))
        .filter(group => group.entryIds.length > 0));
      setSelectedIds(current => {
        const next = new Set(current);
        next.delete(id);
        return next;
      });
    } catch {
      // ignore remove failures
    }
  };

  const removeSelectedEntries = () => {
    for (const id of selectedIds()) void removeEntry(id);
    setSelectedIds(new Set<string>());
  };

  const createContentGroup = () => {
    const selected = [...selectedIds()];
    if (selected.length === 0) return;
    const nextGroup: ContentGroupData = {
      id: `cg-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`,
      name: "New Group",
      collapsed: false,
      entryIds: selected,
    };
    const nextGroups = groups()
      .map(group => ({ ...group, entryIds: group.entryIds.filter(id => !selectedIds().has(id)) }))
      .filter(group => group.entryIds.length > 0);
    nextGroups.push(nextGroup);
    setGroups(nextGroups);
    setSelectedIds(new Set<string>());
    void saveGroups(nextGroups);
  };

  const removeContentGroup = (groupId: string) => {
    const nextGroups = groups().filter(group => group.id !== groupId);
    setGroups(nextGroups);
    void saveGroups(nextGroups);
  };

  const toggleContentGroupCollapsed = (groupId: string) => {
    const nextGroups = groups().map(group =>
      group.id === groupId ? { ...group, collapsed: !group.collapsed } : group
    );
    setGroups(nextGroups);
    void saveGroups(nextGroups);
  };

  const startContentGroupRename = (groupId: string, name: string) => {
    setEditingGroupId(groupId);
    setGroupNameDraft(name);
  };

  const commitContentGroupRename = (groupId: string) => {
    const draft = groupNameDraft().trim();
    if (draft) {
      const nextGroups = groups().map(group =>
        group.id === groupId ? { ...group, name: draft } : group
      );
      setGroups(nextGroups);
      void saveGroups(nextGroups);
    }
    setEditingGroupId(null);
  };

  const engine = useDragEngine({
    containerRef: () => listContainerRef,
    getItems: () => contentTLItems().map((item): DragItem =>
      item.kind === "entry"
        ? { kind: "row", id: item.entry.id }
        : { kind: "group", id: item.id }
    ),
    onCommit: (fromId, dropId, fromKind) => commitContentDrop(fromId, dropId, fromKind),
  });

  const isDraggingGroup = () => engine.draggingKind() === "group";

  const detectContentDropTarget = (cursorY: number): string | null => {
    const items = contentTLItems();
    const heights = engine.cachedHeights();
    const tops = engine.cachedTops();
    const dragId = engine.draggingId()!;
    const groupDrag = isDraggingGroup();
    const dragTlId = groupDrag ? `group:${dragId}` : dragId;
    const scrollDelta = engine.scrollDelta();

    for (const item of items) {
      const id = ctlId(item);
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
          const draggedIsGroupMember = groupByEntryId().get(dragId) === groupId;
          const edgePx = Math.min(20, height * 0.15);
          if (!draggedIsGroupMember && relY < edgePx) return `tl-group:${groupId}`;
          if (!draggedIsGroupMember && relY > height - edgePx) return `tl-group-after:${groupId}`;

          const midYs = engine.cachedMidYs();
          const candidates = item.entries.filter(entry => entry.id !== dragTlId);
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

        const entryId = item.entry.id;
        return cursorY >= y + height / 2 ? `row-after:${entryId}` : entryId;
      }
    }
    return null;
  };

  const handleStartDrag = (id: string, kind: "row" | "group", event: PointerEvent | MouseEvent) => {
    engine.startDrag(id, kind, event);
    const onMoveOverride = (pointerEvent: PointerEvent) => {
      if (!engine.draggingId()) return;
      const target = detectContentDropTarget(pointerEvent.clientY);
      if (target !== null) engine.setHoveredDropId(target);
    };
    const onUpCleanup = () => {
      window.removeEventListener("pointermove", onMoveOverride);
      window.removeEventListener("pointerup", onUpCleanup);
    };
    window.addEventListener("pointermove", onMoveOverride);
    window.addEventListener("pointerup", onUpCleanup);
  };

  const previewTLItems = createMemo(() => {
    const items = contentTLItems();
    const dragging = engine.draggingId();
    const drop = engine.hoveredDropId();
    if (!dragging || !drop) return items;
    if (drop.startsWith("group-drop:")) return items;
    const dragTlId = isDraggingGroup() ? `group:${dragging}` : dragging;
    const draggingItem = items.find(item => ctlId(item) === dragTlId);
    if (!draggingItem) return items;
    const rest = items.filter(item => ctlId(item) !== dragTlId);
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
      const idx = rest.findIndex(item => item.kind === "entry" && item.entry.id === rowId);
      insertIdx = idx < 0 ? rest.length : idx + 1;
    } else {
      const idx = rest.findIndex(item => item.kind === "entry" && item.entry.id === drop);
      insertIdx = idx < 0 ? rest.length : idx;
    }

    const next = [...rest];
    next.splice(insertIdx, 0, draggingItem);
    return next;
  });

  const previewTranslates = createMemo(() => {
    if (!engine.draggingId() || !engine.hoveredDropId() || engine.hoveredDropId()?.startsWith("group-drop:")) {
      return new Map<string, number>();
    }
    return computePreviewTranslates(
      contentTLItems().map(ctlId),
      previewTLItems().map(ctlId),
      engine.cachedHeights(),
      0
    );
  });

  const previewGroupRowTranslates = createMemo(() => {
    const map = new Map<string, number>();
    const dragging = engine.draggingId();
    const drop = engine.hoveredDropId();
    if (!dragging || !drop) return map;

    for (const item of contentTLItems()) {
      if (item.kind !== "group") continue;
      const ids = item.entries.map(entry => entry.id);
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

  const commitContentGroupMove = (groupId: string, dropId: string) => {
    const group = groups().find(item => item.id === groupId);
    if (!group) return;
    const groupEntryIds = new Set(group.entryIds);
    const groupEntries = group.entryIds
      .map(id => entries().find(entry => entry.id === id))
      .filter((entry): entry is ContentEntry => Boolean(entry));
    const otherEntries = entries().filter(entry => !groupEntryIds.has(entry.id));

    let insertIdx: number;
    if (dropId.startsWith("tl-group:")) {
      const targetGroupId = dropId.slice("tl-group:".length);
      const targetGroup = groups().find(item => item.id === targetGroupId);
      const first = targetGroup?.entryIds[0];
      insertIdx = first ? otherEntries.findIndex(entry => entry.id === first) : otherEntries.length;
      if (insertIdx < 0) insertIdx = otherEntries.length;
    } else if (dropId.startsWith("tl-group-after:")) {
      const targetGroupId = dropId.slice("tl-group-after:".length);
      const targetGroup = groups().find(item => item.id === targetGroupId);
      let last = -1;
      if (targetGroup) {
        for (let i = otherEntries.length - 1; i >= 0; i--) {
          if (targetGroup.entryIds.includes(otherEntries[i].id)) {
            last = i;
            break;
          }
        }
      }
      insertIdx = last < 0 ? otherEntries.length : last + 1;
    } else if (dropId.startsWith("row-after:")) {
      const rowId = dropId.slice("row-after:".length);
      const idx = otherEntries.findIndex(entry => entry.id === rowId);
      insertIdx = idx < 0 ? otherEntries.length : idx + 1;
    } else {
      const idx = otherEntries.findIndex(entry => entry.id === dropId);
      insertIdx = idx < 0 ? otherEntries.length : idx;
    }

    const nextEntries = [...otherEntries];
    nextEntries.splice(insertIdx, 0, ...groupEntries);
    setEntries(nextEntries);
    void saveOrder(nextEntries);
  };

  const commitContentRowMove = (fromId: string, dropId: string) => {
    const nextGroups = groups().map(group => ({ ...group, entryIds: [...group.entryIds] }));
    const membership = groupByEntryId();
    const srcGroupId = membership.get(fromId) ?? null;
    if (srcGroupId) {
      const sourceGroup = nextGroups.find(group => group.id === srcGroupId);
      if (sourceGroup) sourceGroup.entryIds = removeFromArr(sourceGroup.entryIds, fromId);
    }

    if (dropId.startsWith("group-drop:")) {
      const targetGroupId = dropId.slice("group-drop:".length);
      const targetGroup = nextGroups.find(group => group.id === targetGroupId);
      if (targetGroup && !targetGroup.entryIds.includes(fromId)) targetGroup.entryIds.push(fromId);

      const entry = entries().find(item => item.id === fromId);
      if (!entry) return;
      const others = entries().filter(item => item.id !== fromId);
      let lastIdx = -1;
      if (targetGroup) {
        for (let i = others.length - 1; i >= 0; i--) {
          if (targetGroup.entryIds.includes(others[i].id)) {
            lastIdx = i;
            break;
          }
        }
      }
      const insertIdx = lastIdx < 0 ? others.length : lastIdx + 1;
      const nextEntries = [...others];
      nextEntries.splice(insertIdx, 0, entry);
      const filteredGroups = nextGroups.filter(group => group.entryIds.length > 0);
      batch(() => {
        setGroups(filteredGroups);
        setEntries(nextEntries);
      });
      void saveOrder(nextEntries);
      void saveGroups(filteredGroups);
      return;
    }

    let targetRowId: string | null = null;
    if (dropId.startsWith("row-after:")) targetRowId = dropId.slice("row-after:".length);
    else if (!dropId.startsWith("tl-group:") && !dropId.startsWith("tl-group-after:")) targetRowId = dropId;
    if (targetRowId) {
      const targetGroupId = membership.get(targetRowId);
      if (targetGroupId) {
        const targetGroup = nextGroups.find(group => group.id === targetGroupId);
        if (targetGroup && !targetGroup.entryIds.includes(fromId)) {
          const targetIdx = targetGroup.entryIds.indexOf(targetRowId);
          if (dropId.startsWith("row-after:")) {
            targetGroup.entryIds.splice(targetIdx < 0 ? targetGroup.entryIds.length : targetIdx + 1, 0, fromId);
          } else {
            targetGroup.entryIds.splice(targetIdx < 0 ? 0 : targetIdx, 0, fromId);
          }
        }
      }
    }

    const entry = entries().find(item => item.id === fromId);
    if (!entry) return;
    const others = entries().filter(item => item.id !== fromId);
    const filteredGroups = nextGroups.filter(group => group.entryIds.length > 0);

    let insertIdx: number;
    if (dropId.startsWith("tl-group:")) {
      const targetGroupId = dropId.slice("tl-group:".length);
      const targetGroup = filteredGroups.find(group => group.id === targetGroupId);
      const first = targetGroup?.entryIds[0];
      insertIdx = first ? others.findIndex(item => item.id === first) : others.length;
      if (insertIdx < 0) insertIdx = others.length;
    } else if (dropId.startsWith("tl-group-after:")) {
      const targetGroupId = dropId.slice("tl-group-after:".length);
      const targetGroup = filteredGroups.find(group => group.id === targetGroupId);
      let last = -1;
      if (targetGroup) {
        for (let i = others.length - 1; i >= 0; i--) {
          if (targetGroup.entryIds.includes(others[i].id)) {
            last = i;
            break;
          }
        }
      }
      insertIdx = last < 0 ? others.length : last + 1;
    } else if (dropId.startsWith("row-after:")) {
      const rowId = dropId.slice("row-after:".length);
      const idx = others.findIndex(item => item.id === rowId);
      insertIdx = idx < 0 ? others.length : idx + 1;
    } else {
      const idx = others.findIndex(item => item.id === dropId);
      insertIdx = idx < 0 ? others.length : idx;
    }

    const nextEntries = [...others];
    nextEntries.splice(insertIdx, 0, entry);
    batch(() => {
      setGroups(filteredGroups);
      setEntries(nextEntries);
    });
    void saveOrder(nextEntries);
    void saveGroups(filteredGroups);
  };

  const commitContentDrop = (fromId: string, dropId: string, fromKind: "row" | "group") => {
    if (fromId === dropId) return;
    if (fromKind === "group") commitContentGroupMove(fromId, dropId);
    else commitContentRowMove(fromId, dropId);
  };

  const draggingEntry = () => {
    const id = engine.draggingId();
    if (!id || isDraggingGroup()) return null;
    return entries().find(entry => entry.id === id) ?? null;
  };

  const draggingEntryIcon = () => {
    const entry = draggingEntry();
    return entry ? meta().get(entry.id)?.iconUrl : undefined;
  };

  const draggingGroupItem = () => {
    const id = engine.draggingId();
    if (!id || !isDraggingGroup()) return null;
    return contentTLItems().find(item => item.kind === "group" && item.id === id) as (ContentTopLevelItem & { kind: "group" }) | undefined ?? null;
  };

  const toggleSelect = (id: string) => {
    setSelectedIds(current => {
      const next = new Set(current);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const clearSelection = () => setSelectedIds(new Set<string>());

  const selectedCount = () => selectedIds().size;

  const advancedEntry = () => {
    const id = advancedEntryId();
    return id ? entries().find(entry => entry.id === id) ?? null : null;
  };

  const updateEntryRules = (entryId: string, rules: ContentEntry["versionRules"]) => {
    setEntries(current => current.map(entry => entry.id === entryId ? { ...entry, versionRules: rules } : entry));
  };

  const isEntryResolved = (entry: ContentEntry): boolean | null => {
    if (entry.versionRules.length === 0) return true;
    const mcVersion = selectedMcVersion();
    const loader = selectedModLoader();
    for (const rule of entry.versionRules) {
      const versionMatch = rule.mcVersions.length === 0 || rule.mcVersions.some(version => mcVersionMatches(version, mcVersion));
      const loaderMatch = rule.loader === "any" || rule.loader === loader;
      if (rule.kind === "exclude" && versionMatch && loaderMatch) return false;
      if (rule.kind === "only" && !(versionMatch && loaderMatch)) return false;
    }
    return true;
  };

  return {
    advancedEntry,
    clearSelection,
    commitContentGroupRename,
    contentTLItems,
    ctlId,
    createContentGroup,
    draggingEntry,
    draggingEntryIcon,
    draggingGroupItem,
    editingGroupId,
    engine,
    entries,
    groupNameDraft,
    groups,
    handleStartDrag,
    isDraggingGroup,
    isEntryResolved,
    label,
    meta,
    previewGroupRowTranslates,
    previewTranslates,
    removeContentGroup,
    removeEntry,
    removeSelectedEntries,
    selectedCount,
    selectedIds,
    setAdvancedEntryId,
    setGroupNameDraft,
    setListContainerRef,
    startContentGroupRename,
    toggleContentGroupCollapsed,
    toggleSelect,
    updateEntryRules,
  };
}
