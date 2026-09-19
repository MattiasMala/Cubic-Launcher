import { Show, createSignal, onCleanup, onMount } from "solid-js";
import { MaterialIcon, PackageIcon } from "../icons";
import { contentLookupFailures, selectedMcVersion } from "../../store";
import { contentCompatibility, contentProjects } from "../../lib/content-meta";
import type { ContentEntry, ContentMeta } from "./content-types";

const DRAG_THRESHOLD = 5;

interface ContentEntryRowProps {
  entry: ContentEntry;
  /** The category this row belongs to: half of the key the pre-check reports failures under. */
  contentType: string;
  info?: ContentMeta;
  isResolved: boolean | null;
  isSelected: boolean;
  onAdvanced: () => void;
  onStartDrag: (id: string, event: PointerEvent) => void;
  onToggleSelected: () => void;
}

export function ContentEntryRow(props: ContentEntryRowProps) {
  const [pendingClickPos, setPendingClickPos] = createSignal<{ x: number; y: number } | null>(null);

  const isLocal = () => props.entry.source === "local";

  // D62: the entry sits in the list and the launch installs nothing for it,
  // and until now only `launcher.log` said so, once, during a launch. The
  // answer comes from the project metadata the tab already fetches — the same
  // single `GET /v2/projects` — so the badge costs no request of its own.
  //
  // A local pack has no Modrinth versions to have or lack, so it is never
  // asked about.
  const compatibility = () =>
    isLocal()
      ? "compatible"
      : contentCompatibility(contentProjects().get(props.entry.id), selectedMcVersion());

  // D63, and the reason it is here and not only in the popup: the popup opens
  // only when there is at least one update, so a pack the pre-check could not
  // ask about would stay invisible in exactly the case that matters. The row
  // reads what the last pre-check reported and fetches nothing.
  const precheckError = () => contentLookupFailures().get(`${props.contentType}/${props.entry.id}`);

  const handlePointerDown = (event: PointerEvent) => {
    if (event.button !== 0) return;
    event.preventDefault();
    event.stopPropagation();
    setPendingClickPos({ x: event.clientX, y: event.clientY });
  };

  onMount(() => {
    const onMove = (event: PointerEvent) => {
      const pending = pendingClickPos();
      if (!pending) return;
      const dx = Math.abs(event.clientX - pending.x);
      const dy = Math.abs(event.clientY - pending.y);
      if (dx > DRAG_THRESHOLD || dy > DRAG_THRESHOLD) {
        setPendingClickPos(null);
        props.onStartDrag(props.entry.id, event);
      }
    };
    const onUp = () => {
      if (!pendingClickPos()) return;
      props.onToggleSelected();
      setPendingClickPos(null);
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
    onCleanup(() => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
    });
  });

  const stopDragPropagation = (event: MouseEvent | PointerEvent) => event.stopPropagation();

  return (
    <div
      class={`group flex items-center gap-3 rounded-md px-2 py-2 transition-colors select-none cursor-grab active:cursor-grabbing ${
        props.isSelected ? "bg-primary/10 ring-1 ring-primary/20" : "hover:bg-muted/50"
      }`}
      onPointerDown={handlePointerDown}
    >
      <div class="flex h-8 w-8 shrink-0 items-center justify-center overflow-hidden rounded-md bg-muted">
        <Show when={props.info?.iconUrl} fallback={<PackageIcon class="h-4 w-4 text-muted-foreground" />}>
          <img src={props.info!.iconUrl} alt="" class="h-8 w-8 object-cover" loading="lazy" />
        </Show>
      </div>
      <div class="min-w-0 flex-1">
        <div class="flex flex-wrap items-center gap-1.5">
          <span class={`truncate font-medium ${
            props.isResolved === true ? "text-green-400" : props.isResolved === false ? "text-red-400" : "text-foreground"
          }`}>
            {props.info?.name ?? props.entry.id}
          </span>
          <span class={`inline-flex items-center rounded-md border px-1.5 py-0.5 text-[10px] font-medium ${
            isLocal() ? "border-warning/40 bg-warning/10 text-warning" : "border-border bg-secondary text-secondary-foreground"
          }`}>
            {isLocal() ? "Local" : "Modrinth"}
          </span>
          {/* D60: no version for this target. D63: nobody could ask — two
              different sentences, deliberately not one. */}
          <Show when={compatibility() === "no-version"}>
            <span
              class="inline-flex items-center gap-1 rounded-md border border-warning/40 bg-warning/10 px-1.5 py-0.5 text-[10px] font-medium text-warning"
              title={`Modrinth publishes no version of this pack for Minecraft ${selectedMcVersion()}. A launch on this target installs nothing for it.`}
            >
              No {selectedMcVersion()} version
            </span>
          </Show>
          <Show when={compatibility() === "unknown"}>
            <span
              class="inline-flex items-center gap-1 rounded-md border border-border bg-muted px-1.5 py-0.5 text-[10px] font-medium text-muted-foreground"
              title="Modrinth could not be reached for this pack, so whether it has a version for this target is unknown."
            >
              Compatibility unknown
            </span>
          </Show>
          <Show when={precheckError()}>
            <span
              class="inline-flex items-center gap-1 rounded-md border border-border bg-muted px-1.5 py-0.5 text-[10px] font-medium text-muted-foreground"
              title={`The last update check could not read this pack's versions from Modrinth (${precheckError()}). Whether it has an update is unknown; the launch keeps what you have.`}
            >
              Update check failed
            </span>
          </Show>
        </div>
      </div>
      <div class="flex shrink-0 items-center gap-1 opacity-0 group-hover:opacity-100 transition-opacity">
        <button
          onClick={(event) => {
            event.stopPropagation();
            props.onAdvanced();
          }}
          onPointerDown={stopDragPropagation}
          onMouseDown={stopDragPropagation}
          class="flex h-7 items-center gap-1 rounded-md px-2 text-xs font-medium text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
          title="Advanced settings"
        >
          <MaterialIcon name="settings" size="sm" />
          Advanced
        </button>
      </div>
    </div>
  );
}
