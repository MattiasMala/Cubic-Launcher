import { For, Show } from "solid-js";
import { ContentAdvancedPanel } from "./ContentAdvancedPanel";
import { ContentEntryRow } from "./ContentEntryRow";
import type { ContentTabViewProps } from "./content-types";
import { useContentTabState } from "./use-content-tab-state";
import {
  ChevronDownIcon,
  ChevronRightIcon,
  FolderOpenIcon,
  MaterialIcon,
  PackageIcon,
  XIcon,
} from "../icons";

export { bumpContentVersion, seedContentName } from "./use-content-tab-state";

export function ContentTabView(props: ContentTabViewProps) {
  const state = useContentTabState(props);

  return (
    <div class="flex flex-1 flex-col overflow-hidden">
      <Show when={state.advancedEntry()}>
        <ContentAdvancedPanel
          entry={state.advancedEntry()!}
          name={state.meta().get(state.advancedEntry()!.id)?.name ?? state.advancedEntry()!.id}
          modlistName={props.modlistName}
          contentType={props.type}
          onClose={() => state.setAdvancedEntryId(null)}
          onUpdate={state.updateEntryRules}
        />
      </Show>

      <Show when={state.selectedCount() > 0} fallback={
        <div class="px-6 py-2 bg-bgPanel border-b border-borderColor shrink-0 flex items-center gap-3">
          <button
            onClick={props.onAddContent}
            class="px-4 py-1.5 rounded-lg bg-primary hover:bg-brandPurpleHover text-white text-sm font-medium flex items-center gap-2 transition-colors duration-75"
          >
            <MaterialIcon name="add" size="md" />
            Add {state.label()}
          </button>
        </div>
      }>
        <header class="h-14 bg-primary/20 border-b border-primary flex items-center px-6 justify-between shrink-0">
          <div class="flex items-center gap-4">
            <span class="text-sm font-medium text-primary border-r border-primary/30 pr-4">
              {state.selectedCount()} selected
            </span>
            <div class="flex items-center gap-2">
              <button
                onClick={state.createContentGroup}
                class="flex items-center gap-1.5 px-3 py-1.5 text-sm font-medium text-primary hover:text-white hover:bg-primary/40 rounded-lg border border-primary/30 transition-colors duration-75"
              >
                <MaterialIcon name="folder_open" size="md" />
                Create Group
              </button>
              <button
                onClick={state.removeSelectedEntries}
                class="flex items-center gap-1.5 px-3 py-1.5 text-sm font-medium text-primary hover:text-white hover:bg-primary/40 rounded-lg border border-primary/30 transition-colors duration-75"
              >
                <MaterialIcon name="delete" size="md" />
                Delete
              </button>
            </div>
          </div>
          <button
            onClick={state.clearSelection}
            class="text-primary hover:text-white p-1 transition-colors duration-75"
          >
            <MaterialIcon name="close" size="lg" />
          </button>
        </header>
      </Show>

      <div class="flex-1 p-4" style={{ overflow: "auto" }}>
        <Show when={state.entries().length > 0} fallback={
          <div class="flex flex-col items-center justify-center py-16 text-center">
            <div class="mb-4 flex h-16 w-16 items-center justify-center rounded-full bg-muted">
              <PackageIcon class="h-8 w-8 text-muted-foreground" />
            </div>
            <h3 class="mb-2 text-lg font-semibold text-foreground">No {state.label()}</h3>
            <p class="mb-6 max-w-xs text-sm text-muted-foreground">
              Add {state.label().toLowerCase()} from Modrinth or upload local files.
            </p>
            <button
              onClick={props.onAddContent}
              class="rounded-md bg-primary px-4 py-2 text-sm font-medium text-primary-foreground transition-colors hover:bg-primary/90"
            >
              Add Your First {state.label().replace(/s$/, "")}
            </button>
          </div>
        }>
          <>
            <Show when={state.engine.draggingId() && state.engine.dragPointer()}>
              <div
                class="pointer-events-none fixed z-50 w-80 rounded-md border border-primary/40 bg-card shadow-2xl ring-1 ring-primary/20"
                style={{
                  left: `${state.engine.dragPointer()!.x + 12}px`,
                  top: `${state.engine.dragPointer()!.y - 24}px`,
                  opacity: "0.95",
                }}
              >
                <Show
                  when={state.isDraggingGroup() && state.draggingGroupItem()}
                  fallback={
                    <div class="flex items-center gap-3 px-2 py-2">
                      <div class="flex h-8 w-8 shrink-0 items-center justify-center overflow-hidden rounded-md bg-muted">
                        <Show when={state.draggingEntryIcon()} fallback={<PackageIcon class="h-4 w-4 text-muted-foreground" />}>
                          <img src={state.draggingEntryIcon()!} alt="" class="h-8 w-8 object-cover" />
                        </Show>
                      </div>
                      <div class="min-w-0 flex-1">
                        <span class="truncate font-medium text-foreground">
                          {state.meta().get(state.draggingEntry()?.id ?? "")?.name ?? state.draggingEntry()?.id}
                        </span>
                      </div>
                    </div>
                  }
                >
                  <div class="flex items-center gap-3 px-3 py-2">
                    <FolderOpenIcon class="h-5 w-5 shrink-0 text-primary" />
                    <div class="min-w-0 flex-1">
                      <div class="truncate font-medium text-foreground">{state.draggingGroupItem()!.name}</div>
                      <div class="text-xs text-muted-foreground">{state.draggingGroupItem()!.entries.length} items</div>
                    </div>
                  </div>
                </Show>
              </div>
            </Show>

            <div class="space-y-0.5" ref={state.setListContainerRef}>
              <For each={state.contentTLItems()}>
                {(item) => {
                  const id = state.ctlId(item);
                  const isDragging = () => {
                    const draggingId = state.engine.draggingId();
                    if (!draggingId) return false;
                    return state.isDraggingGroup() ? `group:${draggingId}` === id : draggingId === id;
                  };
                  const offset = () => isDragging() ? 0 : (state.previewTranslates().get(id) ?? 0);

                  if (item.kind === "entry") {
                    return (
                      <div
                        data-draggable-id={id}
                        data-draggable-mid-id={id}
                        style={{
                          transform: state.engine.anyDragging() ? `translateY(${offset()}px)` : "none",
                          transition: state.engine.anyDragging() ? "transform 150ms ease" : "none",
                          position: "relative",
                          "z-index": isDragging() ? "0" : "1",
                        }}
                        class={isDragging() ? "opacity-0 pointer-events-none" : ""}
                      >
                        <ContentEntryRow
                          entry={item.entry}
                          contentType={props.type}
                          info={state.meta().get(item.entry.id)}
                          isResolved={state.isEntryResolved(item.entry)}
                          isSelected={state.selectedIds().has(item.entry.id)}
                          onAdvanced={() => state.setAdvancedEntryId(item.entry.id)}
                          onStartDrag={(entryId, event) => state.handleStartDrag(entryId, "row", event)}
                          onToggleSelected={() => state.toggleSelect(item.entry.id)}
                        />
                      </div>
                    );
                  }

                  const group = item;
                  const isGroupDrop = () => state.engine.hoveredDropId() === `group-drop:${group.id}`;
                  const isBeforeTarget = () => state.engine.hoveredDropId() === `tl-group:${group.id}`;
                  const isAfterTarget = () => state.engine.hoveredDropId() === `tl-group-after:${group.id}`;

                  return (
                    <div
                      data-draggable-id={id}
                      style={{
                        transform: state.engine.anyDragging() ? `translateY(${offset()}px)` : "none",
                        transition: state.engine.anyDragging() ? "transform 150ms ease" : "none",
                        position: "relative",
                        "z-index": isDragging() ? "0" : "1",
                      }}
                      class={`mb-2 ${isDragging() ? "opacity-0 pointer-events-none" : ""}`}
                    >
                      <Show when={isBeforeTarget()}>
                        <div class="mb-1 h-0.5 rounded bg-primary" />
                      </Show>

                      <div class={`rounded-xl border p-2 shadow-sm transition-colors ${
                        isGroupDrop() ? "border-primary/40 bg-primary/5" : "border-border/70 bg-muted/10"
                      }`}>
                        <div class="flex items-center gap-2 px-1 py-1">
                          <button
                            onClick={() => state.toggleContentGroupCollapsed(group.id)}
                            class="flex items-center text-sm font-medium text-muted-foreground transition-colors hover:text-foreground"
                          >
                            <Show when={group.collapsed} fallback={<ChevronDownIcon class="h-4 w-4" />}>
                              <ChevronRightIcon class="h-4 w-4" />
                            </Show>
                          </button>
                          <div
                            class="cursor-grab touch-none"
                            onPointerDown={(event) => state.handleStartDrag(group.id, "group", event)}
                          >
                            <FolderOpenIcon class="h-4 w-4 text-primary" />
                          </div>
                          <Show
                            when={state.editingGroupId() === group.id}
                            fallback={
                              <span
                                class="flex-1 cursor-pointer text-sm font-medium text-foreground"
                                onClick={() => state.startContentGroupRename(group.id, group.name)}
                              >
                                {group.name}
                              </span>
                            }
                          >
                            <input
                              type="text"
                              value={state.groupNameDraft()}
                              onInput={event => state.setGroupNameDraft(event.currentTarget.value)}
                              onBlur={() => state.commitContentGroupRename(group.id)}
                              onKeyDown={event => {
                                if (event.key === "Enter" || event.key === "Escape") {
                                  state.commitContentGroupRename(group.id);
                                }
                              }}
                              class="flex-1 rounded bg-transparent text-sm font-medium text-foreground outline-none"
                              autofocus
                            />
                          </Show>
                          <span class="shrink-0 text-xs text-muted-foreground">{group.entries.length} items</span>
                          <button
                            onClick={() => state.removeContentGroup(group.id)}
                            class="flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-muted-foreground hover:bg-destructive/10 hover:text-destructive"
                          >
                            <XIcon class="h-3.5 w-3.5" />
                          </button>
                        </div>
                        <Show when={!group.collapsed}>
                          <div class="mt-1 space-y-0.5">
                            <For each={group.entries}>
                              {(entry) => {
                                const isDraggingRow = () =>
                                  state.engine.draggingId() === entry.id && state.engine.draggingKind() === "row";
                                const rowOffset = () => isDraggingRow() ? 0 : (state.previewGroupRowTranslates().get(entry.id) ?? 0);
                                const isTarget = () =>
                                  !isDraggingRow() && (
                                    state.engine.hoveredDropId() === entry.id ||
                                    state.engine.hoveredDropId() === `row-after:${entry.id}`
                                  );
                                return (
                                  <div
                                    data-draggable-id={entry.id}
                                    data-draggable-mid-id={entry.id}
                                    style={{
                                      transform: state.engine.anyDragging() ? `translateY(${rowOffset()}px)` : "none",
                                      transition: state.engine.anyDragging() ? "transform 150ms ease" : "none",
                                      position: "relative",
                                      "z-index": isDraggingRow() ? "0" : "1",
                                    }}
                                    class={isDraggingRow() ? "opacity-0 pointer-events-none" : isTarget() ? "rounded-md ring-1 ring-primary/40" : ""}
                                  >
                                    <ContentEntryRow
                                      entry={entry}
                                      contentType={props.type}
                                      info={state.meta().get(entry.id)}
                                      isResolved={state.isEntryResolved(entry)}
                                      isSelected={state.selectedIds().has(entry.id)}
                                      onAdvanced={() => state.setAdvancedEntryId(entry.id)}
                                      onStartDrag={(entryId, event) => state.handleStartDrag(entryId, "row", event)}
                                      onToggleSelected={() => state.toggleSelect(entry.id)}
                                    />
                                  </div>
                                );
                              }}
                            </For>
                            <Show when={group.entries.length === 0}>
                              <div class="rounded-md border border-dashed border-border bg-background/40 px-4 py-4 text-center text-sm text-muted-foreground">
                                Drag items here to place them in this group.
                              </div>
                            </Show>
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
          </>
        </Show>
      </div>
    </div>
  );
}
