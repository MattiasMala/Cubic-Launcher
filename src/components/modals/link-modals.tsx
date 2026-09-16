import { For, Show } from "solid-js";
import { XIcon } from "../icons";
import { ModIcon } from "../ModIcon";
import {
  linkModalOpen,
  setLinkModalOpen,
  linkModalModIds,
  draftLinks,
  setDraftLinks,
  saveDraftLinks,
  savedLinks,
  setSavedLinks,
  linksOverviewOpen,
  setLinksOverviewOpen,
  rowMap,
} from "../../store";
import { Modal, ModalHeader } from "./modal-base";

type LinkDirection = "a-to-b" | "mutual" | "b-to-a" | "none";

function directionButtonClass(active: boolean) {
  return active
    ? "bg-primary/20 text-primary ring-1 ring-primary/30"
    : "text-muted-foreground hover:bg-muted";
}

export function LinkModal() {
  const pairs = () => {
    const ids = linkModalModIds();
    const result: Array<[string, string]> = [];
    for (let i = 0; i < ids.length; i++) {
      for (let j = i + 1; j < ids.length; j++) {
        result.push([ids[i], ids[j]]);
      }
    }
    return result;
  };

  const hasLink = (from: string, to: string) =>
    draftLinks().some(link => link.fromId === from && link.toId === to);

  const setDirection = (a: string, b: string, dir: LinkDirection) => {
    setDraftLinks(current => {
      const without = current.filter(link =>
        !((link.fromId === a && link.toId === b) || (link.fromId === b && link.toId === a))
      );
      if (dir === "none") return without;
      if (dir === "a-to-b") return [...without, { fromId: a, toId: b }];
      if (dir === "b-to-a") return [...without, { fromId: b, toId: a }];
      return [...without, { fromId: a, toId: b }, { fromId: b, toId: a }];
    });
  };

  const currentDirection = (a: string, b: string): LinkDirection => {
    const ab = hasLink(a, b);
    const ba = hasLink(b, a);
    if (ab && ba) return "mutual";
    if (ab) return "a-to-b";
    if (ba) return "b-to-a";
    return "none";
  };

  const toggleDirection = (a: string, b: string, target: Exclude<LinkDirection, "none">) => {
    const current = currentDirection(a, b);
    setDirection(a, b, current === target ? "none" : target);
  };

  return (
    <Show when={linkModalOpen()}>
      <Modal onClose={() => setLinkModalOpen(false)}>
        <ModalHeader title="Link Mods" description="Define dependency relationships between selected mods." onClose={() => setLinkModalOpen(false)} />
        <div class="flex-1 overflow-y-auto space-y-3 p-6">
          <For each={pairs()}>
            {([a, b]) => {
              const nameA = () => rowMap().get(a)?.name ?? a;
              const nameB = () => rowMap().get(b)?.name ?? b;
              return (
                <div class="flex items-center gap-3 rounded-md border border-border bg-background p-3">
                  <span class="min-w-0 flex-1 truncate text-right text-sm font-medium text-foreground">{nameA()}</span>
                  <div class="flex shrink-0 items-center gap-1">
                    <button
                      onClick={() => toggleDirection(a, b, "a-to-b")}
                      class={`rounded-md px-2 py-1 text-xs font-medium transition-colors ${directionButtonClass(currentDirection(a, b) === "a-to-b")}`}
                      title={`${nameA()} requires ${nameB()}`}
                    >
                      &rarr;
                    </button>
                    <button
                      onClick={() => toggleDirection(a, b, "mutual")}
                      class={`rounded-md px-2 py-1 text-xs font-medium transition-colors ${directionButtonClass(currentDirection(a, b) === "mutual")}`}
                      title={`${nameA()} and ${nameB()} require each other`}
                    >
                      &harr;
                    </button>
                    <button
                      onClick={() => toggleDirection(a, b, "b-to-a")}
                      class={`rounded-md px-2 py-1 text-xs font-medium transition-colors ${directionButtonClass(currentDirection(a, b) === "b-to-a")}`}
                      title={`${nameB()} requires ${nameA()}`}
                    >
                      &larr;
                    </button>
                  </div>
                  <span class="min-w-0 flex-1 truncate text-sm font-medium text-foreground">{nameB()}</span>
                </div>
              );
            }}
          </For>
        </div>
        <div class="flex justify-end gap-2 border-t border-border px-6 py-4">
          <button onClick={() => setLinkModalOpen(false)} class="rounded-md bg-secondary px-4 py-2 text-sm text-secondary-foreground hover:bg-secondary/80">Cancel</button>
          <button onClick={saveDraftLinks} class="rounded-md bg-primary px-4 py-2 text-sm font-medium text-primary-foreground hover:bg-primary/90">Save</button>
        </div>
      </Modal>
    </Show>
  );
}

export function LinksOverviewModal() {
  const pairs = () => {
    const links = savedLinks();
    const seen = new Set<string>();
    const result: Array<{ a: string; b: string }> = [];
    for (const link of links) {
      const key = [link.fromId, link.toId].sort().join("|");
      if (!seen.has(key)) {
        seen.add(key);
        const [a, b] = [link.fromId, link.toId].sort();
        result.push({ a, b });
      }
    }
    return result;
  };

  const nameOf = (id: string) => rowMap().get(id)?.name ?? id;

  const hasLink = (from: string, to: string) =>
    savedLinks().some(link => link.fromId === from && link.toId === to);

  const currentDirection = (a: string, b: string): LinkDirection => {
    const ab = hasLink(a, b);
    const ba = hasLink(b, a);
    if (ab && ba) return "mutual";
    if (ab) return "a-to-b";
    if (ba) return "b-to-a";
    return "none";
  };

  const setDirection = (a: string, b: string, dir: LinkDirection) => {
    setSavedLinks(current => {
      const without = current.filter(link =>
        !((link.fromId === a && link.toId === b) || (link.fromId === b && link.toId === a))
      );
      if (dir === "none") return without;
      if (dir === "a-to-b") return [...without, { fromId: a, toId: b }];
      if (dir === "b-to-a") return [...without, { fromId: b, toId: a }];
      return [...without, { fromId: a, toId: b }, { fromId: b, toId: a }];
    });
  };

  const toggleDirection = (a: string, b: string, target: Exclude<LinkDirection, "none">) => {
    const current = currentDirection(a, b);
    setDirection(a, b, current === target ? "none" : target);
  };

  return (
    <Show when={linksOverviewOpen()}>
      <Modal onClose={() => setLinksOverviewOpen(false)} maxWidth="max-w-lg">
        <ModalHeader title="Link Relations" description="All dependency links defined across your mod list." onClose={() => setLinksOverviewOpen(false)} />
        <div class="max-h-96 flex-1 space-y-2 overflow-y-auto p-4">
          <Show when={pairs().length > 0} fallback={<p class="py-6 text-center text-sm text-muted-foreground">No links defined.</p>}>
            <For each={pairs()}>
              {({ a, b }) => (
                <div class="flex items-center gap-3 rounded-md border border-border bg-background p-3">
                  <span class="min-w-0 flex-1 truncate text-right text-sm font-medium text-foreground">
                    <span class="inline-flex items-center justify-end gap-1">
                      <ModIcon row={rowMap().get(a)} name={nameOf(a)} />
                      {nameOf(a)}
                    </span>
                  </span>
                  <div class="flex shrink-0 items-center gap-1">
                    <button
                      onClick={() => toggleDirection(a, b, "a-to-b")}
                      class={`rounded-md px-2 py-1 text-xs font-medium transition-colors ${directionButtonClass(currentDirection(a, b) === "a-to-b")}`}
                      title={`${nameOf(a)} requires ${nameOf(b)}`}
                    >
                      &rarr;
                    </button>
                    <button
                      onClick={() => toggleDirection(a, b, "mutual")}
                      class={`rounded-md px-2 py-1 text-xs font-medium transition-colors ${directionButtonClass(currentDirection(a, b) === "mutual")}`}
                      title={`${nameOf(a)} and ${nameOf(b)} require each other`}
                    >
                      &harr;
                    </button>
                    <button
                      onClick={() => toggleDirection(a, b, "b-to-a")}
                      class={`rounded-md px-2 py-1 text-xs font-medium transition-colors ${directionButtonClass(currentDirection(a, b) === "b-to-a")}`}
                      title={`${nameOf(b)} requires ${nameOf(a)}`}
                    >
                      &larr;
                    </button>
                  </div>
                  <span class="min-w-0 flex-1 truncate text-sm font-medium text-foreground">
                    <span class="inline-flex items-center gap-1">
                      <ModIcon row={rowMap().get(b)} name={nameOf(b)} />
                      {nameOf(b)}
                    </span>
                  </span>
                  <button
                    onClick={() => setDirection(a, b, "none")}
                    class="flex h-6 w-6 shrink-0 items-center justify-center rounded text-muted-foreground transition-colors hover:bg-destructive/10 hover:text-destructive"
                    title="Remove link"
                  >
                    <XIcon class="h-3.5 w-3.5" />
                  </button>
                </div>
              )}
            </For>
          </Show>
        </div>
        <div class="flex justify-end border-t border-border px-4 py-3">
          <button onClick={() => setLinksOverviewOpen(false)} class="rounded-md bg-secondary px-4 py-2 text-sm text-secondary-foreground hover:bg-secondary/80">
            Close
          </button>
        </div>
      </Modal>
    </Show>
  );
}
