/**
 * Unified drag-and-drop engine for ModListEditor, AltSection, and AlternativesPanel.
 *
 * All layout measurements are taken ONCE at dragStart and cached in signals.
 * During the drag only the container is measured again, to know how far the
 * page under the cursor has scrolled since the start.
 * hoveredDropId is "sticky" — never reset to null mid-drag.
 */
import { createSignal, createMemo, onMount, onCleanup, batch } from "solid-js";
import { computePreviewTranslates } from "./dragUtils";

// ── Types ────────────────────────────────────────────────────────────────────

export type DragItem =
  | { kind: "row"; id: string }
  | { kind: "group"; id: string };

export type DragEngineConfig = {
  containerRef: () => HTMLElement | undefined;
  getItems: () => DragItem[];
  onCommit: (fromId: string, dropId: string, fromKind: "row" | "group") => void;
  /**
   * The drop target under the cursor, for lists whose targets are not the
   * plain `before:`/`after:` ids (groups with inner rows, alt groups). It is
   * the only detector: both pointer moves and scrolls ask it. When a separate
   * pointermove listener did it, a scroll (the autoscroll at the edges) wrote
   * `before:`/`after:` ids that no commit knew, and the row ended up last and
   * out of its group (audit 101, editor-logica-6).
   */
  detect?: (cursorY: number) => string | null;
};

const dragItemId = (item: DragItem): string =>
  item.kind === "row" ? item.id : `group:${item.id}`;

// ── Engine ───────────────────────────────────────────────────────────────────

export function useDragEngine(config: DragEngineConfig) {
  // ── Signals ──────────────────────────────────────────────────────────────
  const [draggingId,   setDraggingId]   = createSignal<string | null>(null);
  const [draggingKind, setDraggingKind] = createSignal<"row" | "group" | null>(null);
  const [hoveredDropId, setHoveredDropId] = createSignal<string | null>(null);
  const [dragPointer,  setDragPointer]  = createSignal<{ x: number; y: number } | null>(null);

  // Cached layout snapshots — taken once at dragStart
  const [cachedContainerRect, setCachedContainerRect] = createSignal<DOMRect | null>(null);
  const [cachedHeights,       setCachedHeights]       = createSignal<Map<string, number>>(new Map());
  const [cachedTops,          setCachedTops]           = createSignal<Map<string, number>>(new Map());
  const [cachedMidYs,         setCachedMidYs]         = createSignal<Map<string, number>>(new Map());
  // Scroll offset delta since drag started (viewport-relative positions shift by this).
  const [scrollDelta,         setScrollDelta]         = createSignal(0);

  const anyDragging = () => draggingId() != null;

  // ── Measurement (once at dragStart) ──────────────────────────────────────

  function measureAll() {
    const container = config.containerRef();
    if (!container) return;

    const rect = container.getBoundingClientRect();
    setCachedContainerRect(rect);

    const heights = new Map<string, number>();
    const tops = new Map<string, number>();
    container.querySelectorAll<HTMLElement>("[data-draggable-id]").forEach(el => {
      const id = el.getAttribute("data-draggable-id")!;
      const r = el.getBoundingClientRect();
      heights.set(id, r.height);
      tops.set(id, r.top);
    });
    setCachedHeights(heights);
    setCachedTops(tops);

    const midYs = new Map<string, number>();
    container.querySelectorAll<HTMLElement>("[data-draggable-mid-id]").forEach(el => {
      const id = el.getAttribute("data-draggable-mid-id")!;
      const r = el.getBoundingClientRect();
      midYs.set(id, r.top + r.height / 2);
    });
    setCachedMidYs(midYs);
  }

  // ── Drop target detection (uses only cached measurements) ────────────────

  function detectDropTarget(cursorY: number): string | null {
    const cr = cachedContainerRect();
    if (!cr) return null;

    const items   = config.getItems();
    const heights = cachedHeights();
    const tops    = cachedTops();
    const dragId  = draggingId()!;
    const sd      = scrollDelta();

    for (const item of items) {
      const id = dragItemId(item);
      if (id === dragId) continue;

      const rawY = tops.get(id);
      const h = heights.get(id) ?? 40;
      if (rawY === undefined) continue;
      const y = rawY - sd; // adjust for scroll since drag started

      if (cursorY < y + h) {
        const isBottom = cursorY >= y + h / 2;
        return isBottom ? `after:${id}` : `before:${id}`;
      }
    }

    // Past the end — treat as after last item
    if (items.length > 0) {
      const lastId = dragItemId(items[items.length - 1]);
      if (lastId !== dragId) return `after:${lastId}`;
      if (items.length >= 2) {
        return `after:${dragItemId(items[items.length - 2])}`;
      }
    }
    return null;
  }

  // ── Preview computation (all createMemo) ─────────────────────────────────

  const previewItems = createMemo((): DragItem[] => {
    const items = config.getItems();
    const dId   = draggingId();
    const drop  = hoveredDropId();
    if (!dId || !drop) return items;

    const draggingItem = items.find(i => dragItemId(i) === dId);
    if (!draggingItem) return items;

    const rest = items.filter(i => dragItemId(i) !== dId);

    let insertIdx: number;
    if (drop.startsWith("before:")) {
      const targetId = drop.slice("before:".length);
      const idx = rest.findIndex(i => dragItemId(i) === targetId);
      insertIdx = idx < 0 ? rest.length : idx;
    } else if (drop.startsWith("after:")) {
      const targetId = drop.slice("after:".length);
      const idx = rest.findIndex(i => dragItemId(i) === targetId);
      insertIdx = idx < 0 ? rest.length : idx + 1;
    } else {
      // Custom drop IDs (group-drop, etc.) — handled by extended engines
      return items;
    }

    const result = [...rest];
    result.splice(insertIdx, 0, draggingItem);
    return result;
  });

  const previewTranslates = createMemo((): Map<string, number> => {
    const dId  = draggingId();
    const drop = hoveredDropId();
    if (!dId || !drop) return new Map();

    const items   = config.getItems();
    const preview = previewItems();
    return computePreviewTranslates(
      items.map(dragItemId),
      preview.map(dragItemId),
      cachedHeights(),
      0  // no fallback — all heights must be measured
    );
  });

  // ── Lifecycle ────────────────────────────────────────────────────────────

  function startDrag(id: string, kind: "row" | "group", event: PointerEvent | MouseEvent) {
    event.preventDefault();
    event.stopPropagation();

    measureAll();
    setScrollDelta(0);

    batch(() => {
      setDraggingId(id);
      setDraggingKind(kind);
      setHoveredDropId(null);
      setDragPointer({ x: event.clientX, y: event.clientY });
    });
    document.body.style.userSelect = "none";
  }

  function finishDrag() {
    const fromId = draggingId();
    const dropId = hoveredDropId();
    const kind   = draggingKind();
    if (fromId && dropId && kind) {
      config.onCommit(fromId, dropId, kind);
    }
    batch(() => {
      setDraggingId(null);
      setDraggingKind(null);
      setHoveredDropId(null);
      setDragPointer(null);
      setCachedContainerRect(null);
      setCachedHeights(new Map());
      setCachedTops(new Map());
      setCachedMidYs(new Map());
    });
    document.body.style.userSelect = "";
  }

  const detect = (cursorY: number): string | null =>
    config.detect ? config.detect(cursorY) : detectDropTarget(cursorY);

  onMount(() => {
    // How far the list moved up since dragStart, whichever ancestor scrolled:
    // the scroll container is not always the container's parent (an AltSection
    // inside the editor, the Fallback Order modal).
    const updateScrollDelta = () => {
      const container = config.containerRef();
      const startRect = cachedContainerRect();
      if (container && startRect) setScrollDelta(startRect.top - container.getBoundingClientRect().top);
    };
    const onMove = (event: PointerEvent) => {
      if (!draggingId()) return;
      setDragPointer({ x: event.clientX, y: event.clientY });
      updateScrollDelta();
      const newTarget = detect(event.clientY);
      if (newTarget !== null) {
        setHoveredDropId(newTarget);
      }
    };
    const onScroll = () => {
      if (!draggingId()) return;
      updateScrollDelta();
      const ptr = dragPointer();
      if (ptr) {
        const newTarget = detect(ptr.y);
        if (newTarget !== null) setHoveredDropId(newTarget);
      }
    };
    const onUp = () => {
      if (draggingId()) finishDrag();
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup",   onUp);
    window.addEventListener("scroll", onScroll, true); // capture phase to catch all scrolls
    onCleanup(() => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup",   onUp);
      window.removeEventListener("scroll", onScroll, true);
      document.body.style.userSelect = "";
    });
  });

  return {
    startDrag,
    draggingId,
    draggingKind,
    hoveredDropId,
    dragPointer,
    anyDragging,
    previewTranslates,
    previewItems,
    // Expose cached measurements for advanced detection in components
    cachedHeights,
    cachedTops,
    cachedMidYs,
    cachedContainerRect,
    scrollDelta,
    setHoveredDropId,
  };
}
