import { For, Show } from "solid-js";
import {
  commitGroupRename,
  editingGroupId,
  groupNameDraft,
  removeAestheticGroup,
  setGroupNameDraft,
  startGroupRename,
  toggleGroupCollapsed,
} from "../../store";
import { ChevronDownIcon, ChevronRightIcon, PackageIcon, XIcon } from "../icons";

/** Where each tile crops its own icon (decision D52): tile n shows quadrant n. */
const QUADRANTS = ["left top", "right top", "left bottom", "right bottom"] as const;

interface ModListEditorGroupHeaderProps {
  groupId: string;
  name: string;
  blockCount: number;
  /**
   * Icons of the first four mods in the group, in order; an entry is absent
   * when that mod has no icon. Resolved by the caller, which is the only place
   * that holds the group's rows.
   */
  iconUrls: Array<string | undefined>;
  collapsed: boolean;
  onStartDrag: (event: PointerEvent) => void;
  enabled: boolean;
  onToggleEnabled: () => void;
}

export function ModListEditorGroupHeader(props: ModListEditorGroupHeaderProps) {
  const editing = () => editingGroupId() === props.groupId;

  return (
    <div class="flex flex-1 items-center gap-2 min-w-0">
      <button
        onClick={event => {
          let element: HTMLElement | null = event.currentTarget as HTMLElement;
          while (element && element !== document.body) {
            const overflowY = getComputedStyle(element).overflowY;
            if (overflowY === "auto" || overflowY === "scroll") break;
            element = element.parentElement;
          }
          const scrollTop = element?.scrollTop ?? 0;
          toggleGroupCollapsed(props.groupId);
          requestAnimationFrame(() => {
            if (element) element.scrollTop = scrollTop;
          });
        }}
        class="flex items-center text-sm font-medium text-muted-foreground transition-colors hover:text-foreground"
        title={props.collapsed ? "Expand group" : "Collapse group"}
      >
        <Show when={props.collapsed} fallback={<ChevronDownIcon class="h-4 w-4" />}>
          <ChevronRightIcon class="h-4 w-4" />
        </Show>
      </button>
      {/* Drag handle: a 2x2 mosaic of the group's first four mod icons (D51: 24 px,
          12 px tiles, which leaves the header height unchanged), each tile cropped
          to its own quadrant. An empty group keeps a glyph — there is nothing to
          show. */}
      <div class="cursor-grab touch-none" onPointerDown={props.onStartDrag} title="Drag to reorder group">
        <Show when={props.blockCount > 0} fallback={<PackageIcon class="h-6 w-6 text-muted-foreground" />}>
          <div class="grid h-6 w-6 grid-cols-2 grid-rows-2 overflow-hidden rounded-sm">
            <For each={[0, 1, 2, 3]}>
              {index => (
                <div
                  class="bg-muted bg-no-repeat"
                  style={props.iconUrls[index]
                    ? {
                        "background-image": `url("${props.iconUrls[index]}")`,
                        "background-size": "200%",
                        "background-position": QUADRANTS[index],
                      }
                    : undefined}
                />
              )}
            </For>
          </div>
        </Show>
      </div>
      <Show
        when={editing()}
        fallback={
          <span
            class="flex-1 cursor-pointer text-sm font-medium text-foreground"
            onClick={() => startGroupRename(props.groupId, props.name)}
          >
            {props.name}
          </span>
        }
      >
        <input
          type="text"
          value={groupNameDraft()}
          onInput={event => setGroupNameDraft(event.currentTarget.value)}
          onBlur={() => commitGroupRename(props.groupId)}
          onKeyDown={event => {
            if (event.key === "Enter" || event.key === "Escape") commitGroupRename(props.groupId);
          }}
          class="flex-1 rounded bg-transparent text-sm font-medium text-foreground outline-none"
          autofocus
        />
      </Show>
      <span class="shrink-0 text-xs text-muted-foreground">{props.blockCount} mods</span>
      <button
        onClick={props.onToggleEnabled}
        class={`flex h-4 w-7 items-center rounded-full px-[3px] transition-colors ${props.enabled ? "bg-green-500/80" : "bg-muted"}`}
        title={props.enabled ? "Group enabled — click to disable all mods" : "Group disabled — click to enable all mods"}
      >
        <div class={`h-2.5 w-2.5 rounded-full bg-white shadow transition-transform ${props.enabled ? "translate-x-[12px]" : "translate-x-0"}`} />
      </button>
      <button
        onClick={() => removeAestheticGroup(props.groupId)}
        class="flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-muted-foreground hover:bg-destructive/10 hover:text-destructive"
        title="Remove group"
      >
        <XIcon class="h-3.5 w-3.5" />
      </button>
    </div>
  );
}
