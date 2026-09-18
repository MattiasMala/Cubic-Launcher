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

/**
 * The mosaic always fills the 24 px square; how it is cut up depends on how many
 * mods the group has (decision D59): 1 → the whole icon, 2 → two full-height
 * halves, 3 → two quarters over a full-width band, 4 (and more: the first four)
 * → the four quadrants.
 *
 * Each region shows *its own* icon's matching region (D52 generalized): the icon
 * is scaled so that its width spans the whole 24 px mosaic — that is what `size`
 * says, as a percentage of the region's own width — and `position` anchors the
 * region's share of it. A square icon therefore fills its region exactly;
 * the icon is cropped, never shrunk into the region (D53).
 */
interface MosaicRegion {
  /** grid placement inside the shared 2x2 grid */
  span: string;
  /** `background-size`: 24 px over the region's width */
  size: string;
  /** `background-position`: which part of the scaled icon this region shows */
  position: string;
}

const MOSAIC_LAYOUTS: Record<number, MosaicRegion[]> = {
  1: [{ span: "col-span-2 row-span-2", size: "100%", position: "center" }],
  2: [
    { span: "row-span-2", size: "200%", position: "left center" },
    { span: "row-span-2", size: "200%", position: "right center" },
  ],
  3: [
    { span: "", size: "200%", position: "left top" },
    { span: "", size: "200%", position: "right top" },
    { span: "col-span-2", size: "100%", position: "center bottom" },
  ],
  4: [
    { span: "", size: "200%", position: "left top" },
    { span: "", size: "200%", position: "right top" },
    { span: "", size: "200%", position: "left bottom" },
    { span: "", size: "200%", position: "right bottom" },
  ],
};

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
  const regions = () => MOSAIC_LAYOUTS[Math.min(props.blockCount, 4)];

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
      {/* Drag handle: the mosaic of the group's first four mod icons, 24 px like
          the glyph it replaces (D51) so the header height does not move. The
          regions come from MOSAIC_LAYOUTS: they always cover the whole square,
          and a mod without an icon leaves its own region — not the rest of the
          mosaic — on `bg-muted`. An empty group keeps the glyph: there is
          nothing to show. */}
      <div class="cursor-grab touch-none" onPointerDown={props.onStartDrag} title="Drag to reorder group">
        <Show when={props.blockCount > 0} fallback={<PackageIcon class="h-6 w-6 text-muted-foreground" />}>
          <div class="grid h-6 w-6 grid-cols-2 grid-rows-2 overflow-hidden rounded-sm">
            <For each={regions()}>
              {(region, index) => (
                <div
                  class={`bg-muted bg-no-repeat ${region.span}`}
                  style={props.iconUrls[index()]
                    ? {
                        "background-image": `url("${props.iconUrls[index()]}")`,
                        "background-size": region.size,
                        "background-position": region.position,
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
