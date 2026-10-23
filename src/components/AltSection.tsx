import { For, Show } from "solid-js";
import { ModRuleItem } from "./ModRuleItem";
import { ChevronDownIcon, ChevronRightIcon, FolderOpenIcon, PackageIcon, XIcon } from "./icons";
import { altTlId, type AltSectionProps } from "./alt-section/types";
import { useAltSectionDrag } from "./alt-section/use-alt-section-drag";
import { offlineSafeImageUrl } from "../store";

export function AltSection(props: AltSectionProps) {
  const drag = useAltSectionDrag(props);

  return (
    <div class="mt-0.5 pb-1" ref={drag.setContainerRef}>
      <Show when={drag.isGroupDrag() && drag.engine.dragPointer()}>
        <div
          class="pointer-events-none fixed z-[100] w-60 rounded-md border border-primary/40 bg-card shadow-2xl ring-1 ring-primary/20"
          style={{ left: `${drag.engine.dragPointer()!.x + 12}px`, top: `${drag.engine.dragPointer()!.y - 16}px` }}
        >
          <div class="flex items-center gap-2 px-3 py-2">
            <FolderOpenIcon class="h-4 w-4 shrink-0 text-primary" />
            <span class="truncate text-sm font-medium text-foreground">{drag.draggingAltGroupData()?.name}</span>
            <span class="ml-1 shrink-0 text-xs text-muted-foreground">{drag.draggingAltGroupData()?.blocks.length} alts</span>
          </div>
        </div>
      </Show>

      <Show when={!drag.isGroupDrag() && drag.engine.draggingId() && drag.engine.dragPointer()}>
        <div
          class="pointer-events-none fixed z-50 w-80 rounded-md border border-primary/40 bg-card shadow-2xl ring-1 ring-primary/20"
          style={{
            left: `${drag.engine.dragPointer()!.x + 12}px`,
            top: `${drag.engine.dragPointer()!.y - 24}px`,
            opacity: "0.95",
          }}
        >
          <div class="flex items-center gap-3 px-2 py-2">
            <div class="flex h-8 w-8 shrink-0 items-center justify-center overflow-hidden rounded-md bg-muted">
              <Show
                when={drag.draggingAlt()?.modrinth_id && offlineSafeImageUrl(drag.modIcons().get(drag.draggingAlt()!.modrinth_id!))}
                fallback={<PackageIcon class="h-4 w-4 text-muted-foreground" />}
              >
                <img
                  src={drag.modIcons().get(drag.draggingAlt()!.modrinth_id!)!}
                  alt={drag.draggingAlt()?.name}
                  class="h-8 w-8 object-cover"
                />
              </Show>
            </div>
            <span class="truncate font-medium text-foreground">{drag.draggingAlt()?.name}</span>
          </div>
        </div>
      </Show>

      <For each={drag.altTopLevelItems()}>
        {(item, tlIndex) => {
          const id = altTlId(item);
          const isDraggingThis = () => {
            const draggingId = drag.engine.draggingId();
            if (!draggingId) return false;
            return drag.isGroupDrag()
              ? item.kind === "alt-group" && item.id === draggingId
              : item.kind === "alt-row" && item.row.id === draggingId;
          };
          const tlOffset = () => isDraggingThis() ? 0 : (drag.previewAltTLTranslates().get(id) ?? 0);
          const isLastTLItem = () => tlIndex() === drag.altTopLevelItems().length - 1;

          if (item.kind === "alt-row") {
            const alt = item.row;
            const isTarget = () =>
              !isDraggingThis() && (
                drag.engine.hoveredDropId() === alt.id ||
                drag.engine.hoveredDropId() === `alt-after:${alt.id}`
              );
            return (
              <div
                data-draggable-id={id}
                data-draggable-mid-id={alt.id}
                style={{
                  transform: drag.engine.anyDragging() ? `translateY(${tlOffset()}px)` : "none",
                  transition: drag.engine.anyDragging() ? "transform 150ms ease" : "none",
                  position: "relative",
                  "z-index": isDraggingThis() ? "0" : "1",
                }}
                class={isDraggingThis() ? "opacity-0 pointer-events-none" : isTarget() ? "rounded-md ring-1 ring-primary/40" : ""}
              >
                <ModRuleItem
                  row={alt}
                  depth={props.depth + 1}
                  isLast={isLastTLItem()}
                  onStartDrag={(altId, e) => drag.handleAltDragStart(altId, "row", e)}
                  onReorderAlts={drag.handleNestedReorderAlts}
                />
              </div>
            );
          }

          const group = item;
          const isGroupDropTarget = () => !drag.isGroupDrag() && drag.engine.hoveredDropId() === `alt-group-drop:${group.id}`;
          const isBeforeTarget = () =>
            drag.engine.hoveredDropId() === `alt-group:${group.id}` ||
            drag.engine.hoveredDropId() === `alt-tl-group:${group.id}`;
          const isAfterTarget = () =>
            drag.engine.hoveredDropId() === `alt-group-after:${group.id}` ||
            drag.engine.hoveredDropId() === `alt-tl-group-after:${group.id}`;

          return (
            <div
              data-draggable-id={id}
              class={`relative ml-6 pl-4 ${isLastTLItem() ? "" : "mb-3"} ${isDraggingThis() ? "opacity-0 pointer-events-none" : ""}`}
              style={{
                transform: drag.engine.anyDragging() && !isDraggingThis() ? `translateY(${tlOffset()}px)` : "none",
                transition: drag.engine.anyDragging() ? "transform 150ms ease" : "none",
                position: "relative",
                "z-index": isDraggingThis() ? "0" : "1",
              }}
            >
              <div class="pointer-events-none absolute inset-y-0 left-0 w-4">
                <Show
                  when={!isLastTLItem()}
                  fallback={<div class="absolute left-2 top-0 h-1/2 w-px bg-border/35" />}
                >
                  <div class="absolute -bottom-3 left-2 top-0 w-px bg-border/35" />
                </Show>
                <div class="absolute left-2 top-1/2 h-px w-2 bg-border/35" />
              </div>

              <Show when={isBeforeTarget()}>
                <div class="mb-1 h-0.5 rounded bg-primary" />
              </Show>

              <div class={`rounded-xl border bg-muted/10 p-2 shadow-sm transition-colors ${
                isGroupDropTarget() ? "border-primary/40 bg-primary/5" : "border-border/70"
              }`}>
                <div class="mb-2 flex items-center gap-2 px-1">
                  <button
                    onClick={() => drag.toggleGroupCollapsed(group.id)}
                    class="flex items-center text-xs font-medium text-muted-foreground transition-colors hover:text-foreground"
                    title={group.collapsed ? "Expand" : "Collapse"}
                  >
                    <Show when={group.collapsed} fallback={<ChevronDownIcon class="h-3.5 w-3.5" />}>
                      <ChevronRightIcon class="h-3.5 w-3.5" />
                    </Show>
                  </button>
                  <div
                    class="cursor-grab touch-none"
                    onPointerDown={(e) => drag.handleAltDragStart(group.id, "group", e)}
                    title="Drag to reorder group"
                  >
                    <FolderOpenIcon class="h-3.5 w-3.5 text-primary" />
                  </div>
                  <Show
                    when={drag.editingGroupId() === group.id}
                    fallback={
                      <span
                        class="flex-1 cursor-pointer text-xs font-medium uppercase tracking-wider text-muted-foreground"
                        onClick={() => drag.startGroupRename(group.id, group.name)}
                      >
                        {group.name}
                      </span>
                    }
                  >
                    <input
                      type="text"
                      value={drag.groupNameDraft()}
                      onInput={e => drag.setGroupNameDraft(e.currentTarget.value)}
                      onBlur={() => drag.commitGroupRename(group.id)}
                      onKeyDown={e => {
                        if (e.key === "Enter" || e.key === "Escape") drag.commitGroupRename(group.id);
                      }}
                      class="flex-1 rounded bg-transparent text-xs font-medium text-muted-foreground outline-none"
                      autofocus
                    />
                  </Show>
                  <span class="text-[10px] text-muted-foreground">{group.blocks.length} alts</span>
                  <button
                    onClick={() => drag.removeAestheticGroup(group.id)}
                    class="flex h-5 w-5 shrink-0 items-center justify-center rounded text-muted-foreground hover:bg-destructive/10 hover:text-destructive"
                    title="Remove group"
                  >
                    <XIcon class="h-3 w-3" />
                  </button>
                </div>

                <Show when={!group.collapsed}>
                  <div class="space-y-1">
                    <For each={group.blocks}>
                      {(alt) => {
                        const isDraggingAlt = () => drag.engine.draggingId() === alt.id && !drag.isGroupDrag();
                        const isTarget = () =>
                          !isDraggingAlt() && (
                            drag.engine.hoveredDropId() === alt.id ||
                            drag.engine.hoveredDropId() === `alt-after:${alt.id}`
                          );
                        const offset = () => isDraggingAlt() ? 0 : (drag.previewGroupedTranslates().get(alt.id) ?? 0);
                        return (
                          <div
                            data-draggable-id={alt.id}
                            data-draggable-mid-id={alt.id}
                            style={{
                              transform: drag.engine.anyDragging() ? `translateY(${offset()}px)` : "none",
                              transition: drag.engine.anyDragging() ? "transform 150ms ease" : "none",
                              position: "relative",
                              "z-index": isDraggingAlt() ? "0" : "1",
                            }}
                            class={isDraggingAlt() ? "opacity-0 pointer-events-none" : isTarget() ? "rounded-md ring-1 ring-primary/40" : ""}
                          >
                            <ModRuleItem
                              row={alt}
                              depth={0}
                              onStartDrag={(altId, e) => drag.handleAltDragStart(altId, "row", e)}
                              onReorderAlts={drag.handleNestedReorderAlts}
                            />
                          </div>
                        );
                      }}
                    </For>
                  </div>
                </Show>
              </div>

              <Show when={isAfterTarget()}>
                <div class="mt-1 h-0.5 rounded bg-primary" />
              </Show>
            </div>
          );
        }}
      </For>
    </div>
  );
}
