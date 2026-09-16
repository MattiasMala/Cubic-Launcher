/**
 * ModRuleItem — renders a single mod rule row.
 *
 * Handles row display (icon, name, tags, actions) and the click-vs-drag threshold.
 * Alt drag-and-drop logic lives in AltSection (rendered below the row when expanded).
 */
import { For, Show, createSignal, onMount, onCleanup } from "solid-js";
import type { ModRow } from "../lib/types";
import {
  selectedIds, expandedRows, modIcons,
  functionalGroupsByBlockId, conflictModIds, conflictPairsForId, rowMap,
  aestheticGroups, removeRowsFromAestheticGroups,
  toggleSelected, toggleExpanded, functionalGroupTagClass, functionalGroupTagStyle,
  removeFunctionalGroupMember,
  tagFilter, functionalGroups, tagFilterForcedExpanded,
  linksByModId, removeLink, cycleLinkDirection, removeIncompatibility,
  setAdvancedPanelModId, setPinModalRowId, pinnedRowIds, topLevelRowIds, parentIdByChildId,
  selectedCount, resolvedModIds, resolvedTarget,
  selectedModListName, selectedMcVersion, selectedModLoader, onToggleEnabled,
} from "../store";
import {
  ChevronRightIcon, AlertTriangleIcon, PackageIcon, XIcon, MaterialIcon,
} from "./icons";
import { AltSection } from "./AltSection";
import { ModIcon } from "./ModIcon";

interface ModRuleItemProps {
  row: ModRow;
  depth?: number;
  isLast?: boolean;
  /** Called when the user starts dragging this row (parent handles the drag session). */
  onStartDrag?: (rowId: string, event: PointerEvent | MouseEvent) => void;
  onReorderAlts?: (parentId: string, orderedIds: string[]) => void;
}

export function ModRuleItem(props: ModRuleItemProps) {
  const depth      = () => props.depth ?? 0;
  const isSelected = () => selectedIds().includes(props.row.id);
  const isExpanded = () =>
    expandedRows().includes(props.row.id) || tagFilterForcedExpanded().has(props.row.id);
  const hasAlts    = () => (props.row.alternatives?.length ?? 0) > 0;
  const isLocal    = () => props.row.kind === "local";
  const hasConflict   = () => conflictModIds().has(props.row.id);
  const conflictPairs = () => conflictPairsForId().get(props.row.id) ?? [];
  const fGroups       = () => functionalGroupsByBlockId().get(props.row.id) ?? [];
  // A Modrinth row draws the CDN icon; a local row draws the icon extracted from
  // its jar (mod_icons.rs). A local jar without an icon keeps the PackageIcon.
  const iconUrl    = () =>
    (props.row.modrinth_id ? modIcons().get(props.row.modrinth_id) : undefined) ?? props.row.iconImage;
  const containingGroup = () =>
    aestheticGroups().find(group => group.blockIds.includes(props.row.id)) ?? null;
  const isResolved = () => {
    const target = resolvedTarget();
    if (
      target === null ||
      target.modlistName !== selectedModListName() ||
      target.mcVersion !== selectedMcVersion() ||
      target.modLoader !== selectedModLoader()
    ) {
      return null;
    }
    const ids = resolvedModIds();
    if (ids === null) return null; // resolution not yet run
    return ids.has(props.row.primaryModId ?? props.row.id);
  };

  const stopDragPropagation = (event: MouseEvent | PointerEvent) => event.stopPropagation();

  const isPinned = () => pinnedRowIds().has(props.row.id);

  /**
   * Why the pin button is disabled, or `null` when it works.
   *
   * A version can be pinned on a top-level Modrinth entry, on the dynamic
   * entry parked under a pin (D48: that replaces the pin), and the button also
   * opens on the pin itself to change or remove it. Anything else is a
   * fallback of another mod: pinning it would pull it out of that chain.
   */
  const pinReason = (): string | null => {
    if (isPinned()) return null;
    if (props.row.kind === "local") return "A local jar has no Modrinth release to pin";
    if (topLevelRowIds().has(props.row.id)) return null;

    const parentId = parentIdByChildId().get(props.row.id);
    const parent = parentId ? rowMap().get(parentId) : undefined;
    if (parent && pinnedRowIds().has(parent.id)) return null;

    return "Only a top-level mod can have a pinned version";
  };

  // Returns true if alt (or any of its descendants) matches the active tag filter.
  const altMatchesFilter = (alt: ModRow): boolean => {
    const tf = tagFilter();
    if (tf.size === 0) return true;
    const fg = functionalGroups();
    const check = (row: ModRow): boolean =>
      fg.some(g => tf.has(g.id) && g.modIds.includes(row.id)) ||
      (row.alternatives ?? []).some(check);
    return check(alt);
  };
  const hasVisibleAlts = () =>
    hasAlts() && (props.row.alternatives ?? []).some(altMatchesFilter);

  // ── Click-vs-drag threshold ────────────────────────────────────────────────
  const DRAG_THRESHOLD = 5;
  const [pendingClickPos, setPendingClickPos] = createSignal<{ x: number; y: number } | null>(null);

  const handleRowPointerDown = (event: PointerEvent) => {
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
        props.onStartDrag?.(props.row.id, event);
      }
    };
    const onUp = () => {
      if (pendingClickPos()) {
        toggleSelected(props.row.id);
        setPendingClickPos(null);
      }
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup",   onUp);
    onCleanup(() => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup",   onUp);
    });
  });

  // ── Render ─────────────────────────────────────────────────────────────────

  return (
    <div class={depth() > 0 ? "relative ml-6 pl-4" : ""}>
      {/* Connector lines for nested depth */}
      <Show when={depth() > 0}>
        <div class="pointer-events-none absolute inset-y-0 left-0 w-4">
          <Show
            when={!props.isLast}
            fallback={<div class="absolute left-2 top-0 h-7 w-px bg-border/35" />}
          >
            <div class="absolute bottom-0 left-2 top-0 w-px bg-border/35" />
          </Show>
          <div class="absolute left-2 top-7 h-px w-3 bg-border/35" />
        </div>
      </Show>

      {/* Main row */}
      <div
        class={`group flex items-center gap-3 rounded-md px-2 py-2 transition-colors select-none ${
          props.onStartDrag ? "cursor-grab active:cursor-grabbing" : "cursor-pointer"
        } ${
          isSelected() ? "bg-primary/10 ring-1 ring-primary/20" : "hover:bg-muted/50"
        }`}
        onPointerDown={handleRowPointerDown}
      >
        {/* Mod icon */}
        <div class="flex h-8 w-8 shrink-0 items-center justify-center overflow-hidden rounded-md bg-muted">
          <Show when={iconUrl()} fallback={<PackageIcon class="h-4 w-4 text-muted-foreground" />}>
            <img
              src={iconUrl()!}
              alt={props.row.name}
              class="h-8 w-8 object-cover"
              onError={e => { e.currentTarget.style.display = "none"; }}
            />
          </Show>
        </div>

        {/* Info */}
        <div class="min-w-0 flex-1">
          <div class="flex flex-wrap items-center gap-1.5">
            <span class={`truncate font-medium ${
              !props.row.enabled ? "text-muted-foreground line-through opacity-50" :
              isResolved() === true ? "text-green-400" : isResolved() === false ? "text-red-400" : "text-muted-foreground"
            }`}>{props.row.name}</span>

            {/* Source badge */}
            <span class={`inline-flex items-center rounded-md border px-1.5 py-0.5 text-[10px] font-medium ${
              isLocal()
                ? "border-warning/40 bg-warning/10 text-warning"
                : "border-border bg-secondary text-secondary-foreground"
            }`}>
              {isLocal() ? "Local" : "Modrinth"}
            </span>

            {/* Local dependency warning */}
            <Show when={isLocal()}>
              <span
                title="Manual mod — verify and add required dependencies yourself"
                class="inline-flex items-center gap-0.5 rounded-md border border-warning/40 bg-warning/10 px-1.5 py-0.5 text-[10px] font-medium text-warning"
              >
                <AlertTriangleIcon class="h-3 w-3" />
                Verify deps
              </span>
            </Show>

            {/* Conflict pills */}
            <Show when={hasConflict()}>
              <For each={conflictPairs()}>
                {pair => {
                  const wins    = () => pair.winnerId === props.row.id;
                  const otherId = () => wins() ? pair.loserId : pair.winnerId;
                  const otherName = () => rowMap().get(otherId())?.name ?? otherId();
                  return (
                    <span
                      title={wins() ? `Wins against ${otherName()}` : `Loses to ${otherName()}`}
                      class={`inline-flex items-center gap-1 rounded-md px-1.5 py-0.5 text-[10px] font-medium ring-1 ${
                        wins()
                          ? "bg-green-500/10 text-green-500 ring-green-500/30"
                          : "bg-red-500/10 text-red-500 ring-red-500/30"
                      }`}
                    >
                      {otherName()}
                      <button
                        onClick={e => { e.stopPropagation(); removeIncompatibility(props.row.id, otherId()); }}
                        onPointerDown={stopDragPropagation}
                        class="opacity-60 hover:opacity-100 transition-opacity"
                        title="Remove incompatibility"
                      >
                        <XIcon class="h-2.5 w-2.5" />
                      </button>
                    </span>
                  );
                }}
              </For>
            </Show>

            {/* Tags */}
            <For each={props.row.tags.filter(t => t !== "Alternative" && t !== "Conflict Set")}>
              {tag => (
                <span class="inline-flex items-center rounded-md border border-border bg-muted px-1.5 py-0.5 text-[10px] text-muted-foreground">
                  {tag}
                </span>
              )}
            </For>

            {/* Functional group tags */}
            <For each={fGroups()}>
              {g => (
                <span class={functionalGroupTagClass(g.tone)} style={functionalGroupTagStyle(g.tone)}>
                  {g.name}
                  <button
                    onClick={e => { e.stopPropagation(); removeFunctionalGroupMember(g.id, props.row.id); }}
                    onPointerDown={stopDragPropagation}
                    onMouseDown={stopDragPropagation}
                    class="ml-0.5 opacity-40 hover:opacity-100 transition-opacity"
                    title={`Remove from "${g.name}"`}
                  >
                    <XIcon class="h-2.5 w-2.5" />
                  </button>
                </span>
              )}
            </For>


            {/* Links badge with dropdown */}
            <Show when={(linksByModId().get(props.row.id) ?? []).length > 0}>
              {(() => {
                const [linksOpen, setLinksOpen] = createSignal(false);
                const myLinks = () => linksByModId().get(props.row.id) ?? [];
                return (
                  <div class="relative inline-block" onPointerDown={stopDragPropagation} onMouseDown={stopDragPropagation}>
                    <button
                      onClick={e => { e.stopPropagation(); setLinksOpen(o => !o); }}
                      class="inline-flex items-center gap-0.5 rounded-md border border-cyan-500/30 bg-cyan-500/10 px-1.5 py-0.5 text-[10px] font-medium text-cyan-400 hover:bg-cyan-500/20 transition-colors"
                    >
                      <MaterialIcon name="link" size="sm" class="-ml-0.5" />
                      Links ({myLinks().length})
                    </button>
                    <Show when={linksOpen()}>
                      <div class="fixed inset-0 z-10" onClick={() => setLinksOpen(false)} />
                      <div class="absolute left-0 top-full mt-1 z-20 min-w-[220px] rounded-lg border border-border bg-card shadow-lg overflow-hidden">
                        <For each={myLinks()}>
                          {link => {
                            const partnerName = () => rowMap().get(link.partnerId)?.name ?? link.partnerId;
                            const arrow = () =>
                              link.direction === "mutual" ? "\u21C4"
                              : link.direction === "requires" ? "\u2192"
                              : "\u2190";
                            return (
                              <div class="flex items-center gap-1.5 px-3 py-1.5 text-xs hover:bg-muted/30">
                                <ModIcon row={props.row} />
                                <span class="truncate text-foreground">{props.row.name}</span>
                                <button
                                  onClick={e => { e.stopPropagation(); cycleLinkDirection(props.row.id, link.partnerId); }}
                                  class="text-cyan-400 shrink-0 font-bold hover:text-cyan-200 hover:bg-cyan-500/20 rounded px-1 transition-colors"
                                  title="Click to change direction"
                                >
                                  {arrow()}
                                </button>
                                <ModIcon row={rowMap().get(link.partnerId)} name={partnerName()} />
                                <span class="truncate text-foreground flex-1">{partnerName()}</span>
                                <button
                                  onClick={e => { e.stopPropagation(); removeLink(props.row.id, link.partnerId); }}
                                  class="shrink-0 text-muted-foreground hover:text-destructive transition-colors"
                                  title={`Remove link`}
                                >
                                  <XIcon class="h-3 w-3" />
                                </button>
                              </div>
                            );
                          }}
                        </For>
                      </div>
                    </Show>
                  </div>
                );
              })()}
            </Show>
          </div>

          <Show when={props.row.modrinth_id && !isLocal()}>
            <div class="mt-0.5 text-xs text-muted-foreground/60">{props.row.modrinth_id}</div>
          </Show>
        </div>

        {/* Actions */}
        <div class="flex shrink-0 items-center gap-1">
          <Show when={hasVisibleAlts()}>
            <button
              onClick={() => toggleExpanded(props.row.id)}
              onPointerDown={stopDragPropagation}
              onMouseDown={stopDragPropagation}
              class={`flex h-7 items-center gap-1 rounded-md px-2 text-xs font-medium transition-colors ${
                isExpanded()
                  ? "bg-primary/15 text-primary"
                  : "text-muted-foreground hover:bg-muted hover:text-foreground"
              }`}
              title={isExpanded() ? "Collapse alternatives" : "Show alternatives"}
            >
              <ChevronRightIcon
                class={`h-3.5 w-3.5 transition-transform duration-150 ${isExpanded() ? "rotate-90" : ""}`}
              />
              {props.row.alternatives!.length} alt{props.row.alternatives!.length > 1 ? "s" : ""}
            </button>
          </Show>

          <Show when={containingGroup()}>
            <button
              onClick={e => {
                let el: HTMLElement | null = e.target as HTMLElement;
                while (el && el !== document.body) {
                  const ov = getComputedStyle(el).overflowY;
                  if (ov === "auto" || ov === "scroll") break;
                  el = el.parentElement;
                }
                const scrollTop = el?.scrollTop ?? 0;
                removeRowsFromAestheticGroups([props.row.id]);
                requestAnimationFrame(() => { if (el) el.scrollTop = scrollTop; });
              }}
              onPointerDown={stopDragPropagation}
              onMouseDown={stopDragPropagation}
              class="flex h-7 items-center gap-1 rounded-md px-2 text-xs font-medium text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
              title={`Remove from ${containingGroup()!.name}`}
            >
              <XIcon class="h-3.5 w-3.5" />
              Ungroup
            </button>
          </Show>

          {/* Pinned version (D3). Only mods have this: content packs use
              ContentEntryRow, which D10 leaves alone. */}
          <button
            onClick={e => { e.stopPropagation(); setPinModalRowId(props.row.id); }}
            onPointerDown={stopDragPropagation}
            onMouseDown={stopDragPropagation}
            disabled={pinReason() !== null}
            class="flex h-7 items-center gap-1 rounded-md px-2 text-xs font-medium text-muted-foreground transition-colors hover:bg-muted hover:text-foreground disabled:opacity-40 disabled:cursor-not-allowed"
            title={pinReason() ?? (isPinned() ? "Change or remove the pinned version" : "Pin this mod to one release")}
          >
            <MaterialIcon name="push_pin" size="sm" />
            {isPinned() ? "Pinned" : "Pin"}
          </button>

          <button
            onClick={e => { e.stopPropagation(); setAdvancedPanelModId(props.row.id); }}
            onPointerDown={stopDragPropagation}
            onMouseDown={stopDragPropagation}
            disabled={selectedCount() >= 2}
            class="flex h-7 items-center gap-1 rounded-md px-2 text-xs font-medium text-muted-foreground transition-colors hover:bg-muted hover:text-foreground disabled:opacity-40 disabled:cursor-not-allowed"
            title="Advanced mod settings"
          >
            <MaterialIcon name="settings" size="sm" />
            Advanced
          </button>

          {/* Enable/disable toggle */}
          <button
            onClick={e => { e.stopPropagation(); const handler = onToggleEnabled(); if (handler) handler(props.row.id, !props.row.enabled); }}
            onPointerDown={stopDragPropagation}
            onMouseDown={stopDragPropagation}
            class={`flex h-4 w-7 items-center rounded-full px-[3px] transition-colors ${props.row.enabled ? "bg-green-500/80" : "bg-muted"}`}
            title={props.row.enabled ? "Enabled — click to disable" : "Disabled — click to enable"}
          >
            <div class={`h-2.5 w-2.5 rounded-full bg-white shadow transition-transform ${props.row.enabled ? "translate-x-[12px]" : "translate-x-0"}`} />
          </button>
        </div>
      </div>

      {/* Alternatives section (handles its own drag state) */}
      <Show when={isExpanded() && hasVisibleAlts()}>
        <AltSection
          parentRow={props.row}
          depth={depth()}
          onReorderAlts={props.onReorderAlts}
        />
      </Show>
    </div>
  );
}
