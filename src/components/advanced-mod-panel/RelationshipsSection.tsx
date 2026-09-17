import { For, Show } from "solid-js";
import { setAdvancedPanelModId, rowMap, parentIdByChildId, openAlternativesPanel } from "../../store";
import type { ModRow } from "../../lib/types";
import { MaterialIcon } from "../icons";
import { ModIcon } from "../ModIcon";
import { SectionHeader } from "./shared";

export function RelationshipsSection(props: { modId: string; row: ModRow }) {
  const parentId = () => parentIdByChildId().get(props.modId);
  const parentRow = () => {
    const id = parentId();
    return id ? rowMap().get(id) : undefined;
  };
  const childRows = () => props.row.alternatives ?? [];

  return (
    <div>
      <SectionHeader title="Relationships" />
      <div class="p-4 space-y-2">
        <Show when={parentRow()}>
          <div class="flex items-center gap-2">
            <span class="text-xs text-muted-foreground shrink-0">Parent:</span>
            <button
              onClick={() => setAdvancedPanelModId(parentId()!)}
              class="inline-flex items-center gap-1 text-sm font-medium text-primary hover:underline truncate"
            >
              <ModIcon row={parentRow()!} />{parentRow()!.name}
            </button>
          </div>
        </Show>
        <Show when={childRows().length > 0}>
          <div class="flex flex-wrap items-center gap-2">
            <span class="text-xs text-muted-foreground shrink-0">Alternatives:</span>
            <For each={childRows()}>
              {child => (
                <button
                  onClick={() => setAdvancedPanelModId(child.id)}
                  class="inline-flex items-center gap-1 text-sm font-medium text-primary hover:underline"
                >
                  <ModIcon row={child} />{child.name}
                </button>
              )}
            </For>
          </div>
        </Show>
        <Show when={!parentRow() && childRows().length === 0}>
          <span class="text-xs text-muted-foreground">No parent or alternatives.</span>
        </Show>
        <button
          onClick={() => openAlternativesPanel(props.modId)}
          class="flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground transition-colors"
        >
          <MaterialIcon name="add" size="sm" />
          Add Alternative
        </button>
      </div>
    </div>
  );
}
