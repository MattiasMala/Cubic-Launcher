import { For, Show } from "solid-js";
import type { ContentTabId } from "../store";
import {
  activeAccount,
  activeContentTab,
  instancePresentation,
  modIcons,
  modListCards,
  modRowsState,
  onToggleEnabled,
  search,
  selectedModListName,
  setActiveContentTab,
  setAddModModalOpen,
  setCreateModlistModalOpen,
  setExportModalOpen,
  setInstancePresentationOpen,
  topLevelItems,
} from "../store";
import { ActionBar } from "./ActionBar";
import { ModRuleItem } from "./ModRuleItem";
import { ContentTabView } from "./mod-list-editor/ContentTabView";
import { ModListEditorEmptyState } from "./mod-list-editor/ModListEditorEmptyState";
import { ModListEditorGroupHeader } from "./mod-list-editor/ModListEditorGroupHeader";
import { tlId, useModListEditorDrag } from "./mod-list-editor/use-mod-list-editor-drag";
import {
  ExternalLinkIcon,
  FolderOpenIcon,
  MaterialIcon,
  PackageIcon,
  PencilIcon,
} from "./icons";

interface Props {
  onAddMod: () => void;
  onDeleteSelected: () => void;
  onReorder: (orderedIds: string[]) => void;
  onReorderAlts?: (parentId: string, orderedIds: string[]) => void;
}

const TABS: Array<{ id: ContentTabId; label: string; icon: string }> = [
  { id: "mods", label: "Mods", icon: "extension" },
  { id: "resourcepack", label: "Resource Packs", icon: "palette" },
  { id: "datapack", label: "Data Packs", icon: "database" },
  { id: "shader", label: "Shaders", icon: "auto_awesome" },
];

export function ModListEditor(props: Props) {
  const activeModList = () => modListCards().find((modList) => modList.name === selectedModListName());
  const hasContent = () => modRowsState().length > 0;

  const {
    draggingGroupItem,
    draggingRow,
    draggingRowIcon,
    engine,
    handleStartDrag,
    isDraggingGroup,
    previewGroupRowTranslates,
    previewTranslates,
    setListContainerRef,
    setScrollContainerRef,
  } = useModListEditorDrag({ onReorder: props.onReorder });

  return (
    <div class="flex flex-1 flex-col overflow-hidden">
      <Show
        when={activeModList()}
        fallback={
          <div class="flex flex-1 items-center justify-center">
            <div class="flex flex-col items-center text-center">
              <div class="mb-4 flex h-16 w-16 items-center justify-center rounded-full bg-muted">
                <PackageIcon class="h-8 w-8 text-muted-foreground" />
              </div>
              <h3 class="text-lg font-semibold text-foreground">No Mod List Selected</h3>
              <p class="mt-1 text-sm text-muted-foreground">
                Select a mod list from the sidebar or create a new one.
              </p>
              <div class="mt-4 flex items-center gap-3">
                <button
                  onClick={() => setCreateModlistModalOpen(true)}
                  class="flex items-center gap-1.5 rounded-md bg-primary px-4 py-2 text-sm font-medium text-primary-foreground hover:bg-primary/90"
                >
                  <MaterialIcon name="add" size="sm" />
                  New
                </button>
                <button
                  onClick={() => {
                    void (async () => {
                      try {
                        const { open } = await import("@tauri-apps/plugin-dialog");
                        const selected = await open({
                          title: "Import Mod List",
                          filters: [
                            { name: "Mod List Archive", extensions: ["zip"] },
                            { name: "Rules JSON", extensions: ["json"] },
                          ],
                          multiple: false,
                        });
                        if (!selected) return;
                        const { invoke } = await import("@tauri-apps/api/core");
                        await invoke("import_modlist_command", { sourcePath: selected as string });
                        window.location.reload();
                      } catch {
                        // cancelled or error
                      }
                    })();
                  }}
                  class="flex items-center gap-1.5 rounded-md bg-secondary px-4 py-2 text-sm font-medium text-secondary-foreground hover:bg-secondary/80"
                >
                  <MaterialIcon name="download" size="sm" />
                  Import
                </button>
              </div>
            </div>
          </div>
        }
      >
        <div class="shrink-0 border-b border-border bg-card/50 px-4 py-3">
          <div class="flex items-start justify-between gap-4">
            <div class="min-w-0 flex-1">
              <h2 class="text-lg font-semibold text-foreground">
                {activeModList()!.displayName || activeModList()!.name}
              </h2>
              <div class="mt-1.5 flex flex-wrap items-center gap-3 text-xs text-muted-foreground">
                <span>{modRowsState().length} rule{modRowsState().length !== 1 ? "s" : ""}</span>
                <span>·</span>
                <span>by {instancePresentation().iconAccent || activeAccount()?.gamertag || "—"}</span>
              </div>
            </div>
            <div class="flex shrink-0 items-center gap-1">
              <button
                onClick={() => setInstancePresentationOpen(true)}
                class="flex h-8 items-center gap-1.5 rounded-md px-2.5 text-xs text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
                title="Edit settings for this mod list"
              >
                <PencilIcon class="h-3.5 w-3.5" />
                Settings
              </button>
              <button
                onClick={() => {
                  void (async () => {
                    try {
                      const { open } = await import("@tauri-apps/plugin-dialog");
                      const selected = await open({
                        title: "Import Mod List",
                        filters: [
                          { name: "Mod List Archive", extensions: ["zip"] },
                          { name: "Rules JSON", extensions: ["json"] },
                        ],
                        multiple: false,
                      });
                      if (!selected) return;
                      const { invoke } = await import("@tauri-apps/api/core");
                      await invoke("import_modlist_command", { sourcePath: selected as string });
                      window.location.reload();
                    } catch {
                      // cancelled or error
                    }
                  })();
                }}
                class="flex h-8 items-center gap-1.5 rounded-md px-2.5 text-xs text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
                title="Import mod list from a zip archive or rules.json"
              >
                <MaterialIcon name="download" size="sm" />
                Import
              </button>
              <button
                onClick={() => setExportModalOpen(true)}
                class="flex h-8 items-center gap-1.5 rounded-md px-2.5 text-xs text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
                title="Export this mod list"
              >
                <ExternalLinkIcon class="h-3.5 w-3.5" />
                Export
              </button>
            </div>
          </div>

          <div class="mt-3 flex gap-1 -mb-3">
            <For each={TABS}>
              {(tab) => {
                const isActive = () => activeContentTab() === tab.id;
                return (
                  <button
                    onClick={() => setActiveContentTab(tab.id)}
                    class={`flex items-center gap-1.5 rounded-t-md border-b-2 px-3 py-2 text-sm font-medium transition-colors ${
                      isActive()
                        ? "border-primary bg-primary/5 text-primary"
                        : "border-transparent text-muted-foreground hover:bg-muted/30 hover:text-foreground"
                    }`}
                  >
                    <MaterialIcon name={tab.icon} size="sm" />
                    {tab.label}
                  </button>
                );
              }}
            </For>
          </div>
        </div>

        <Show
          when={activeContentTab() === "mods"}
          fallback={
            <ContentTabView
              type={activeContentTab() as string}
              modlistName={selectedModListName()}
              onAddContent={() => setAddModModalOpen(true)}
            />
          }
        >
          <ActionBar onAddMod={props.onAddMod} onDeleteSelected={props.onDeleteSelected} />

          <div class="flex-1 p-4" ref={setScrollContainerRef} style={{ overflow: "auto" }}>
            <Show when={hasContent()} fallback={<ModListEditorEmptyState onAddMod={props.onAddMod} />}>
              <>
                <Show when={engine.draggingId() && engine.dragPointer()}>
                  <div
                    class="pointer-events-none fixed z-50 w-80 rounded-md border border-primary/40 bg-card shadow-2xl ring-1 ring-primary/20"
                    style={{
                      left: `${engine.dragPointer()!.x + 12}px`,
                      top: `${engine.dragPointer()!.y - 24}px`,
                      opacity: "0.95",
                    }}
                  >
                    <Show
                      when={isDraggingGroup() && draggingGroupItem()}
                      fallback={
                        <div class="flex items-center gap-3 px-2 py-2">
                          <div class="flex h-8 w-8 shrink-0 items-center justify-center overflow-hidden rounded-md bg-muted">
                            <Show when={draggingRowIcon()} fallback={<PackageIcon class="h-4 w-4 text-muted-foreground" />}>
                              <img src={draggingRowIcon()!} alt={draggingRow()?.name} class="h-8 w-8 object-cover" />
                            </Show>
                          </div>
                          <div class="min-w-0 flex-1">
                            <span class="truncate font-medium text-foreground">{draggingRow()?.name}</span>
                          </div>
                        </div>
                      }
                    >
                      <div class="flex items-center gap-3 px-3 py-2">
                        <FolderOpenIcon class="h-5 w-5 shrink-0 text-primary" />
                        <div class="min-w-0 flex-1">
                          <div class="truncate font-medium text-foreground">{draggingGroupItem()!.name}</div>
                          <div class="text-xs text-muted-foreground">{draggingGroupItem()!.blocks.length} mods</div>
                        </div>
                      </div>
                    </Show>
                  </div>
                </Show>

                <div class="space-y-0.5" ref={setListContainerRef}>
                  <For each={topLevelItems()}>
                    {(item) => {
                      const id = tlId(item);
                      const isDragging = () => {
                        const draggingId = engine.draggingId();
                        if (!draggingId) return false;
                        return isDraggingGroup() ? `group:${draggingId}` === id : draggingId === id;
                      };
                      const offset = () => isDragging() ? 0 : (previewTranslates().get(id) ?? 0);

                      if (item.kind === "row") {
                        return (
                          <div
                            data-draggable-id={id}
                            data-draggable-mid-id={id}
                            style={{
                              transform: engine.anyDragging() ? `translateY(${offset()}px)` : "none",
                              transition: engine.anyDragging() ? "transform 150ms ease" : "none",
                              position: "relative",
                              "z-index": isDragging() ? "0" : "1",
                            }}
                            class={isDragging() ? "pointer-events-none opacity-0" : ""}
                          >
                            <div class={engine.hoveredDropId() === item.row.id ? "rounded-md ring-1 ring-primary/40" : ""}>
                              <ModRuleItem
                                row={item.row}
                                onStartDrag={(rowId, event) => handleStartDrag(rowId, "row", event)}
                                onReorderAlts={(parentId, orderedIds) => props.onReorderAlts?.(parentId, orderedIds)}
                              />
                            </div>
                          </div>
                        );
                      }

                      const group = item;
                      const isGroupDrop = () => engine.hoveredDropId() === `group-drop:${group.id}`;
                      const isBeforeTarget = () => engine.hoveredDropId() === `tl-group:${group.id}`;
                      const isAfterTarget = () => engine.hoveredDropId() === `tl-group-after:${group.id}`;

                      return (
                        <div
                          data-draggable-id={id}
                          style={{
                            transform: engine.anyDragging() ? `translateY(${offset()}px)` : "none",
                            transition: engine.anyDragging() ? "transform 150ms ease" : "none",
                            position: "relative",
                            "z-index": isDragging() ? "0" : "1",
                          }}
                          class={`mb-2 ${isDragging() ? "pointer-events-none opacity-0" : ""}`}
                        >
                          <Show when={isBeforeTarget()}>
                            <div class="mb-1 h-0.5 rounded bg-primary" />
                          </Show>

                          <div class={`rounded-xl border p-2 shadow-sm transition-colors ${
                            isGroupDrop() ? "border-primary/40 bg-primary/5" : "border-border/70 bg-muted/10"
                          }`}>
                            <div class="flex items-center gap-1 px-1 py-1">
                              <ModListEditorGroupHeader
                                groupId={group.id}
                                name={group.name}
                                blockCount={group.blocks.length}
                                iconUrls={group.blocks.slice(0, 4).map(row =>
                                  (row.modrinth_id ? modIcons().get(row.modrinth_id) : undefined) ?? row.iconImage
                                )}
                                collapsed={group.collapsed}
                                onStartDrag={(event) => handleStartDrag(group.id, "group", event)}
                                enabled={group.blocks.every((row) => row.enabled)}
                                onToggleEnabled={() => {
                                  const toggleEnabled = onToggleEnabled();
                                  if (!toggleEnabled) return;
                                  const allEnabled = group.blocks.every((row) => row.enabled);
                                  toggleEnabled(group.blocks.map((row) => row.id), !allEnabled);
                                }}
                              />
                            </div>

                            <Show when={!group.collapsed}>
                              <div class="mt-1 space-y-1">
                                <For each={group.blocks}>
                                  {(row) => {
                                    const isDraggingRow = () => engine.draggingId() === row.id && engine.draggingKind() === "row";
                                    const rowOffset = () => isDraggingRow() ? 0 : (previewGroupRowTranslates().get(row.id) ?? 0);
                                    const isTarget = () =>
                                      !isDraggingRow() && (
                                        engine.hoveredDropId() === row.id ||
                                        engine.hoveredDropId() === `row-after:${row.id}`
                                      );

                                    return (
                                      <div
                                        data-draggable-id={row.id}
                                        data-draggable-mid-id={row.id}
                                        style={{
                                          transform: engine.anyDragging() ? `translateY(${rowOffset()}px)` : "none",
                                          transition: engine.anyDragging() ? "transform 150ms ease" : "none",
                                          position: "relative",
                                          "z-index": isDraggingRow() ? "0" : "1",
                                        }}
                                        class={isDraggingRow() ? "pointer-events-none opacity-0" : isTarget() ? "rounded-md ring-1 ring-primary/40" : ""}
                                      >
                                        <ModRuleItem
                                          row={row}
                                          onStartDrag={(rowId, event) => handleStartDrag(rowId, "row", event)}
                                          onReorderAlts={(parentId, orderedIds) => props.onReorderAlts?.(parentId, orderedIds)}
                                        />
                                      </div>
                                    );
                                  }}
                                </For>
                                <Show when={group.blocks.length === 0}>
                                  <div class="rounded-md border border-dashed border-border bg-background/40 px-4 py-4 text-center text-sm text-muted-foreground">
                                    Drag mods here to place them in this group.
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

                <Show when={search() && topLevelItems().length === 0}>
                  <div class="py-12 text-center">
                    <p class="text-muted-foreground">No mods found matching "{search()}"</p>
                  </div>
                </Show>
              </>
            </Show>
          </div>
        </Show>
      </Show>
    </div>
  );
}
