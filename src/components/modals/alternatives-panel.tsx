import { For, Show, createEffect, createSignal } from "solid-js";
import type { ModRow } from "../../lib/types";
import { appendDebugTrace } from "../../lib/debugTrace";
import { useDragEngine, type DragItem } from "../../lib/dragEngine";
import { GripVerticalIcon, XIcon } from "../icons";
import { ModIcon } from "../ModIcon";
import {
  aestheticGroups,
  setAestheticGroups,
  nextAestheticGroupName,
  alternativesPanelParent,
  setAlternativesPanelParentId,
  rowMap,
} from "../../store";
import { Modal, ModalHeader } from "./modal-base";

function DraggableAltRow(props: {
  alt: ModRow;
  priority: number;
  removing: boolean;
  selected: boolean;
  isDragging: boolean;
  isDropTarget: boolean;
  translateY: number;
  anyDragging: boolean;
  onToggleSelected: () => void;
  onRemove: () => void;
  onOpenAlts: () => void;
  onStartDrag: (e: PointerEvent) => void;
}) {
  return (
    <div
      data-draggable-id={props.alt.id}
      data-draggable-mid-id={props.alt.id}
      style={{
        transform: props.anyDragging ? `translateY(${props.isDragging ? 0 : props.translateY}px)` : "none",
        transition: props.anyDragging ? "transform 150ms ease" : "none",
        position: "relative",
        "z-index": props.isDragging ? "0" : "1",
      }}
      class={`flex items-center gap-3 rounded-md border px-3 py-2.5 ${props.selected ? "border-primary/40 bg-primary/5" : "border-border bg-background"} ${props.isDragging ? "pointer-events-none opacity-0" : ""} ${props.isDropTarget ? "ring-1 ring-primary/40" : ""}`}
    >
      <div
        class="shrink-0 cursor-grab touch-none text-muted-foreground/50 hover:text-muted-foreground"
        onPointerDown={props.onStartDrag}
        title="Drag to reorder"
      >
        <GripVerticalIcon class="h-4 w-4" />
      </div>

      <span class="w-5 shrink-0 text-center text-sm font-mono text-muted-foreground">{props.priority}</span>

      <input
        type="checkbox"
        checked={props.selected}
        onChange={() => props.onToggleSelected()}
        class="h-4 w-4 shrink-0 rounded text-primary"
      />

      <ModIcon row={props.alt} />
      <div class="min-w-0 flex-1">
        <p class="truncate text-sm font-medium text-foreground">{props.alt.name}</p>
        <Show when={props.alt.kind === "local"}>
          <p class="text-xs text-warning">Local JAR - verify dependencies</p>
        </Show>
        <Show when={props.alt.modrinth_id}>
          <p class="text-xs text-muted-foreground/60">{props.alt.modrinth_id}</p>
        </Show>
        <Show when={(props.alt.alternatives?.length ?? 0) > 0}>
          <p class="text-xs text-primary/70">{props.alt.alternatives!.length} sub-alt{props.alt.alternatives!.length !== 1 ? "s" : ""}</p>
        </Show>
      </div>

      <button
        onClick={props.onOpenAlts}
        class="flex h-7 items-center gap-1 rounded-md px-2 text-xs font-medium text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
        title="Manage alternatives of this mod"
      >
        Alts
      </button>

      <button
        onClick={props.onRemove}
        disabled={props.removing}
        class="flex h-7 w-7 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-destructive/10 hover:text-destructive disabled:opacity-50"
        title="Remove - restore as top-level rule"
      >
        <XIcon class="h-3.5 w-3.5" />
      </button>
    </div>
  );
}

interface AlternativesPanelProps {
  onSave: (parentId: string, orderedAltIds: string[]) => Promise<void>;
  onAddAlternative: (parentId: string, altRowId: string) => Promise<void>;
  onRemoveAlternative: (altRowId: string) => Promise<void>;
}

export function AlternativesPanel(props: AlternativesPanelProps) {
  const [ordered, setOrdered] = createSignal<ModRow[]>([]);
  const [saving, setSaving] = createSignal(false);
  const [adding, setAdding] = createSignal(false);
  const [removing, setRemoving] = createSignal<string | null>(null);
  const [selectedAlternativeIds, setSelectedAlternativeIds] = createSignal<string[]>([]);

  createEffect(() => {
    const parent = alternativesPanelParent();
    setOrdered(parent?.alternatives ? [...parent.alternatives] : []);
    setSelectedAlternativeIds([]);
    appendDebugTrace("alts.panel.frontend", {
      parentId: parent?.id ?? null,
      parentName: parent?.name ?? null,
      orderedIds: (parent?.alternatives ?? []).map(alt => alt.id),
    });
  });

  const scopedGroups = () => {
    const parentId = alternativesPanelParent()?.id ?? null;
    if (!parentId) return [];
    return aestheticGroups()
      .filter(group => group.scopeRowId === parentId)
      .map(group => {
        const blockIdSet = new Set(group.blockIds);
        return {
          ...group,
          blocks: ordered().filter(alt => blockIdSet.has(alt.id)),
        };
      })
      .filter(group => group.blocks.length > 0);
  };

  const ungroupedAlternatives = () => {
    const groupedIds = new Set(scopedGroups().flatMap(group => group.blockIds));
    return ordered().filter(alt => !groupedIds.has(alt.id));
  };

  const toggleAlternativeSelection = (altId: string) => {
    setSelectedAlternativeIds(current => current.includes(altId) ? current.filter(id => id !== altId) : [...current, altId]);
  };

  const handleCreateAlternativeGroup = () => {
    const parent = alternativesPanelParent();
    const selectedIds = selectedAlternativeIds().filter(id => ordered().some(alt => alt.id === id));
    if (!parent || selectedIds.length === 0) return;

    const id = `ag-${Date.now()}`;
    const name = nextAestheticGroupName(parent.id);
    setAestheticGroups(current => {
      const withoutSelected = current.map(group => ({
        ...group,
        blockIds: group.blockIds.filter(blockId => !selectedIds.includes(blockId)),
      }));
      return [...withoutSelected, { id, name, collapsed: false, blockIds: selectedIds, scopeRowId: parent.id }];
    });
    setSelectedAlternativeIds([]);
  };

  let altPanelContainerRef: HTMLDivElement | undefined;

  const altPanelEngine = useDragEngine({
    containerRef: () => altPanelContainerRef,
    getItems: () => ordered().map(row => ({ kind: "row", id: row.id }) satisfies DragItem),
    onCommit: (fromId, dropId) => {
      let toId: string;
      if (dropId.startsWith("before:")) {
        toId = dropId.slice("before:".length);
      } else if (dropId.startsWith("after:")) {
        toId = dropId.slice("after:".length);
      } else {
        return;
      }
      if (fromId === toId) return;

      appendDebugTrace("alts.drag.frontend", {
        phase: "start",
        parentId: alternativesPanelParent()?.id ?? null,
        fromId,
        toId,
        orderedIds: ordered().map(alt => alt.id),
      });

      setOrdered(current => {
        const arr = [...current];
        const fromIdx = arr.findIndex(row => row.id === fromId);
        const toIdx = arr.findIndex(row => row.id === toId);
        if (fromIdx === -1 || toIdx === -1) return current;
        const [item] = arr.splice(fromIdx, 1);
        const adjustedToIdx = dropId.startsWith("after:")
          ? (toIdx > fromIdx ? toIdx : toIdx + 1)
          : (toIdx > fromIdx ? toIdx - 1 : toIdx);
        arr.splice(Math.max(0, Math.min(adjustedToIdx, arr.length)), 0, item);
        appendDebugTrace("alts.drag.frontend", {
          phase: "end",
          parentId: alternativesPanelParent()?.id ?? null,
          fromId,
          toId,
          orderedIds: arr.map(alt => alt.id),
        });
        return arr;
      });
    },
  });

  const handleSave = async () => {
    const parent = alternativesPanelParent();
    if (!parent || ordered().length === 0) return;
    appendDebugTrace("alts.save.frontend", {
      parentId: parent.id,
      orderedIds: ordered().map(alt => alt.id),
    });
    setSaving(true);
    await props.onSave(parent.id, ordered().map(row => row.id));
    setSaving(false);
    setAlternativesPanelParentId(null);
  };

  const handleAddAlt = async (altRow: ModRow) => {
    const parent = alternativesPanelParent();
    if (!parent) return;
    appendDebugTrace("alts.add.panel.frontend", {
      parentId: parent.id,
      altRowId: altRow.id,
      altRowName: altRow.name,
    });
    setAdding(true);
    await props.onAddAlternative(parent.id, altRow.id);
    setAdding(false);
  };

  const handleRemoveAlt = async (altRow: ModRow) => {
    appendDebugTrace("alts.remove.panel.frontend", {
      parentId: alternativesPanelParent()?.id ?? null,
      altRowId: altRow.id,
      altRowName: altRow.name,
    });
    setRemoving(altRow.id);
    await props.onRemoveAlternative(altRow.id);
    setRemoving(null);
  };

  const availableToAdd = () => {
    const parent = alternativesPanelParent();
    if (!parent) return [];
    const existingAltIds = new Set(ordered().map(row => row.id));

    const excluded = new Set<string>([parent.id]);
    const parentMap = new Map<string, string>();
    for (const row of rowMap().values()) {
      for (const alt of row.alternatives ?? []) {
        parentMap.set(alt.id, row.id);
      }
    }
    let current = parent.id;
    while (parentMap.has(current)) {
      const parentId = parentMap.get(current)!;
      excluded.add(parentId);
      current = parentId;
    }

    const collectDescendants = (row: ModRow) => {
      for (const alt of row.alternatives ?? []) {
        excluded.add(alt.id);
        collectDescendants(alt);
      }
    };
    collectDescendants(parent);

    return [...rowMap().values()].filter(row => !existingAltIds.has(row.id) && !excluded.has(row.id));
  };

  return (
    <Show when={alternativesPanelParent()}>
      {parent => (
        <Modal onClose={() => setAlternativesPanelParentId(null)} maxWidth="max-w-lg">
          <ModalHeader title={`Fallback Order - ${parent().name}`} description="Drag to reorder. The launcher tries options top to bottom." onClose={() => setAlternativesPanelParentId(null)} />

          <div class="border-b border-border px-6 py-3">
            <p class="mb-1.5 text-xs font-medium uppercase tracking-wider text-muted-foreground">Primary (Priority 1 - fixed)</p>
            <div class="flex items-center gap-3 rounded-md border border-primary/30 bg-primary/5 px-3 py-2">
              <div class="h-4 w-4 shrink-0" />
              <span class="w-5 shrink-0 text-center text-sm font-semibold text-primary">1</span>
              <ModIcon row={parent()} />
              <span class="flex-1 text-sm font-medium text-foreground">{parent().name}</span>
              <span class="text-xs text-muted-foreground">Primary</span>
            </div>
          </div>

          <div class="flex-1 space-y-2 overflow-y-auto p-6">
            <div class="mb-2 flex items-center justify-between gap-3">
              <p class="text-xs font-medium uppercase tracking-wider text-muted-foreground">Fallback order - drag to reorder</p>
              <button
                onClick={handleCreateAlternativeGroup}
                disabled={selectedAlternativeIds().length === 0}
                class="rounded-md bg-secondary px-2.5 py-1 text-xs font-medium text-secondary-foreground hover:bg-secondary/80 disabled:opacity-50"
              >
                Create Group
              </button>
            </div>
            <Show
              when={ordered().length > 0}
              fallback={
                <div class="rounded-md border border-dashed border-border py-6 text-center">
                  <p class="text-sm text-muted-foreground">No fallbacks yet.</p>
                  <p class="mt-1 text-xs text-muted-foreground/60">Add mods from the list below.</p>
                </div>
              }
            >
              <Show when={altPanelEngine.draggingId() && altPanelEngine.dragPointer()}>
                {(() => {
                  const alt = () => ordered().find(row => row.id === altPanelEngine.draggingId());
                  return (
                    <div
                      class="pointer-events-none fixed z-50 flex cursor-grabbing items-center gap-3 rounded-md border border-primary/40 bg-card px-3 py-2.5 shadow-2xl ring-1 ring-primary/20"
                      style={{ left: `${altPanelEngine.dragPointer()!.x + 12}px`, top: `${altPanelEngine.dragPointer()!.y - 16}px`, "min-width": "220px" }}
                    >
                      <GripVerticalIcon class="h-4 w-4 shrink-0 text-primary" />
                      <span class="truncate text-sm font-medium text-foreground">{alt()?.name ?? "..."}</span>
                    </div>
                  );
                })()}
              </Show>

              <div class="space-y-3" ref={altPanelContainerRef}>
                <For each={scopedGroups()}>
                  {group => (
                    <div class="rounded-md border border-border bg-muted/20 p-2">
                      <div class="mb-2 flex items-center justify-between px-1">
                        <span class="text-xs font-medium uppercase tracking-wider text-muted-foreground">{group.name}</span>
                        <span class="text-[10px] text-muted-foreground">{group.blocks.length} mods</span>
                      </div>
                      <div class="space-y-1.5">
                        <For each={group.blocks}>
                          {alt => (
                            <DraggableAltRow
                              alt={alt}
                              priority={ordered().findIndex(candidate => candidate.id === alt.id) + 2}
                              removing={removing() === alt.id}
                              selected={selectedAlternativeIds().includes(alt.id)}
                              isDragging={altPanelEngine.draggingId() === alt.id}
                              isDropTarget={!!altPanelEngine.hoveredDropId()?.endsWith(alt.id)}
                              translateY={altPanelEngine.previewTranslates().get(alt.id) ?? 0}
                              anyDragging={altPanelEngine.anyDragging()}
                              onToggleSelected={() => toggleAlternativeSelection(alt.id)}
                              onRemove={() => void handleRemoveAlt(alt)}
                              onOpenAlts={() => setAlternativesPanelParentId(alt.id)}
                              onStartDrag={e => altPanelEngine.startDrag(alt.id, "row", e)}
                            />
                          )}
                        </For>
                      </div>
                    </div>
                  )}
                </For>

                <For each={ungroupedAlternatives()}>
                  {alt => (
                    <DraggableAltRow
                      alt={alt}
                      priority={ordered().findIndex(candidate => candidate.id === alt.id) + 2}
                      removing={removing() === alt.id}
                      selected={selectedAlternativeIds().includes(alt.id)}
                      isDragging={altPanelEngine.draggingId() === alt.id}
                      isDropTarget={!!altPanelEngine.hoveredDropId()?.endsWith(alt.id)}
                      translateY={altPanelEngine.previewTranslates().get(alt.id) ?? 0}
                      anyDragging={altPanelEngine.anyDragging()}
                      onToggleSelected={() => toggleAlternativeSelection(alt.id)}
                      onRemove={() => void handleRemoveAlt(alt)}
                      onOpenAlts={() => setAlternativesPanelParentId(alt.id)}
                      onStartDrag={e => altPanelEngine.startDrag(alt.id, "row", e)}
                    />
                  )}
                </For>
              </div>
            </Show>

            <Show when={availableToAdd().length > 0}>
              {(() => {
                const [altSearch, setAltSearch] = createSignal("");
                const filteredAlts = () => {
                  const query = altSearch().trim().toLowerCase();
                  return query ? availableToAdd().filter(row => row.name.toLowerCase().includes(query)) : availableToAdd();
                };
                return (
                  <div class="mt-4 border-t border-border pt-4">
                    <p class="mb-2 text-xs font-medium uppercase tracking-wider text-muted-foreground">Add fallback from your mod list</p>
                    <input
                      type="text"
                      placeholder="Search mods..."
                      value={altSearch()}
                      onInput={e => setAltSearch(e.currentTarget.value)}
                      class="mb-2 w-full rounded-md border border-border bg-input px-3 py-1.5 text-sm text-foreground placeholder:text-muted-foreground outline-none focus:ring-1 focus:ring-primary"
                    />
                    <div class="max-h-48 space-y-1.5 overflow-y-auto">
                      <For each={filteredAlts()}>
                        {row => (
                          <div class="flex items-center gap-2 rounded-md border border-border bg-muted/20 px-3 py-2">
                            <ModIcon row={row} />
                            <span class="flex-1 truncate text-sm text-foreground">{row.name}</span>
                            <Show when={row.kind === "local"}>
                              <span class="text-[10px] text-warning">Local</span>
                            </Show>
                            <button
                              onClick={() => void handleAddAlt(row)}
                              disabled={adding()}
                              class="rounded-md bg-secondary px-2.5 py-1 text-xs font-medium text-secondary-foreground transition-colors hover:bg-secondary/80 disabled:opacity-50"
                            >
                              Add
                            </button>
                          </div>
                        )}
                      </For>
                    </div>
                  </div>
                );
              })()}
            </Show>
          </div>

          <div class="flex justify-end gap-2 border-t border-border px-6 py-4">
            <button onClick={() => setAlternativesPanelParentId(null)} class="rounded-md bg-secondary px-4 py-2 text-sm text-secondary-foreground hover:bg-secondary/80">
              Close
            </button>
            <Show when={ordered().length > 0}>
              <button
                onClick={() => void handleSave()}
                disabled={saving()}
                class="rounded-md bg-primary px-4 py-2 text-sm font-medium text-primary-foreground hover:bg-primary/90 disabled:opacity-60"
              >
                {saving() ? "Saving..." : "Save Order"}
              </button>
            </Show>
          </div>
        </Modal>
      )}
    </Show>
  );
}
