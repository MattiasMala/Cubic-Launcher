import { For, Show, createSignal } from "solid-js";
import { savedLinks, setSavedLinks, linksByModId, rowMap } from "../../store";
import type { ModRow } from "../../lib/types";
import { MaterialIcon, XIcon } from "../icons";
import { ModIcon } from "../ModIcon";
import { SectionHeader } from "./shared";

export function LinksSection(props: { modId: string; row: ModRow }) {
  const [addingLink, setAddingLink] = createSignal(false);
  const [newLinkPartnerId, setNewLinkPartnerId] = createSignal("");
  const [newLinkDir, setNewLinkDir] = createSignal<"a-to-b" | "mutual" | "b-to-a">("a-to-b");

  const availableLinkPartners = () => {
    const linked = new Set((linksByModId().get(props.modId) ?? []).map(link => link.partnerId));
    const all = [...rowMap().entries()].filter(([id]) => id !== props.modId && !linked.has(id));
    return all.map(([id, row]) => ({ id, name: row.name }));
  };

  const commitLink = () => {
    const partner = newLinkPartnerId();
    if (!partner) return;
    setSavedLinks(current => {
      const without = current.filter(link => !((link.fromId === props.modId && link.toId === partner) || (link.fromId === partner && link.toId === props.modId)));
      if (newLinkDir() === "a-to-b") return [...without, { fromId: props.modId, toId: partner }];
      if (newLinkDir() === "b-to-a") return [...without, { fromId: partner, toId: props.modId }];
      return [...without, { fromId: props.modId, toId: partner }, { fromId: partner, toId: props.modId }];
    });
    setAddingLink(false);
    setNewLinkPartnerId("");
    setNewLinkDir("a-to-b");
  };

  const myLinks = () => linksByModId().get(props.modId) ?? [];

  const setLinkDirection = (partner: string, dir: "a-to-b" | "mutual" | "b-to-a" | "none") => {
    setSavedLinks(current => {
      const without = current.filter(link => !((link.fromId === props.modId && link.toId === partner) || (link.fromId === partner && link.toId === props.modId)));
      if (dir === "none") return without;
      if (dir === "a-to-b") return [...without, { fromId: props.modId, toId: partner }];
      if (dir === "b-to-a") return [...without, { fromId: partner, toId: props.modId }];
      return [...without, { fromId: props.modId, toId: partner }, { fromId: partner, toId: props.modId }];
    });
  };

  const currentLinkDir = (partnerId: string): "a-to-b" | "mutual" | "b-to-a" | "none" => {
    const links = savedLinks();
    const ab = links.some(link => link.fromId === props.modId && link.toId === partnerId);
    const ba = links.some(link => link.fromId === partnerId && link.toId === props.modId);
    if (ab && ba) return "mutual";
    if (ab) return "a-to-b";
    if (ba) return "b-to-a";
    return "none";
  };

  const toggleLinkDir = (partnerId: string, target: "a-to-b" | "mutual" | "b-to-a") => {
    setLinkDirection(partnerId, currentLinkDir(partnerId) === target ? "none" : target);
  };

  const dirBtnClass = (active: boolean) =>
    `rounded px-1.5 py-0.5 text-xs font-bold transition-colors ${active ? "bg-primary/20 text-primary ring-1 ring-primary/30" : "text-muted-foreground hover:bg-muted"}`;

  return (
    <div>
      <SectionHeader title="Links" />
      <div class="p-4 space-y-2">
        <For each={myLinks()}>
          {link => {
            const partnerName = () => rowMap().get(link.partnerId)?.name ?? link.partnerId;
            const dir = () => currentLinkDir(link.partnerId);
            return (
              <div class="flex items-center gap-2 rounded-md border border-border bg-background p-2">
                <span class="flex items-center gap-1 text-sm font-medium text-foreground truncate min-w-0 flex-1 justify-end"><ModIcon row={props.row} />{props.row.name}</span>
                <div class="flex shrink-0 items-center gap-0.5">
                  <button onClick={() => toggleLinkDir(link.partnerId, "a-to-b")} class={dirBtnClass(dir() === "a-to-b")} title={`${props.row.name} requires ${partnerName()}`}>→</button>
                  <button onClick={() => toggleLinkDir(link.partnerId, "mutual")} class={dirBtnClass(dir() === "mutual")} title="Mutual dependency">↔</button>
                  <button onClick={() => toggleLinkDir(link.partnerId, "b-to-a")} class={dirBtnClass(dir() === "b-to-a")} title={`${partnerName()} requires ${props.row.name}`}>←</button>
                </div>
                <span class="flex items-center gap-1 text-sm font-medium text-foreground truncate min-w-0 flex-1"><ModIcon row={rowMap().get(link.partnerId)} name={partnerName()} />{partnerName()}</span>
                <button
                  onClick={() => setLinkDirection(link.partnerId, "none")}
                  class="shrink-0 flex h-6 w-6 items-center justify-center rounded text-muted-foreground hover:bg-destructive/10 hover:text-destructive transition-colors"
                >
                  <XIcon class="h-3.5 w-3.5" />
                </button>
              </div>
            );
          }}
        </For>
        <Show when={myLinks().length === 0 && !addingLink()}>
          <span class="text-xs text-muted-foreground">No links defined.</span>
        </Show>

        <Show when={addingLink()}>
          <div class="rounded-md border border-primary/30 bg-primary/5 p-3 space-y-2 overflow-hidden">
            <div class="flex items-center gap-2 min-w-0">
              <span class="text-sm font-medium text-foreground truncate shrink-0" style="max-width: 35%">{props.row.name}</span>
              <div class="flex items-center gap-0.5 shrink-0">
                <button onClick={() => setNewLinkDir("a-to-b")} class={dirBtnClass(newLinkDir() === "a-to-b")}>→</button>
                <button onClick={() => setNewLinkDir("mutual")} class={dirBtnClass(newLinkDir() === "mutual")}>↔</button>
                <button onClick={() => setNewLinkDir("b-to-a")} class={dirBtnClass(newLinkDir() === "b-to-a")}>←</button>
              </div>
              {(() => {
                const [linkSearch, setLinkSearch] = createSignal("");
                const filtered = () => {
                  const query = linkSearch().trim().toLowerCase();
                  return query ? availableLinkPartners().filter(partner => partner.name.toLowerCase().includes(query)) : availableLinkPartners();
                };
                return (
                  <div class="flex-1 min-w-0 relative">
                    <input type="text" placeholder="Search mod..." value={linkSearch()} onInput={e => { setLinkSearch(e.currentTarget.value); setNewLinkPartnerId(""); }} class="w-full rounded border border-border bg-input px-2 py-1 text-xs text-foreground placeholder:text-muted-foreground outline-none" />
                    <Show when={linkSearch().trim() && filtered().length > 0}>
                      <div class="absolute left-0 right-0 top-full mt-1 z-20 max-h-32 overflow-y-auto rounded border border-border bg-card shadow-lg">
                        <For each={filtered()}>{partner => <button onClick={() => { setNewLinkPartnerId(partner.id); setLinkSearch(partner.name); }} class="flex w-full items-center gap-1.5 px-2 py-1 text-xs text-foreground hover:bg-muted/50 text-left"><ModIcon row={rowMap().get(partner.id)} name={partner.name} />{partner.name}</button>}</For>
                      </div>
                    </Show>
                  </div>
                );
              })()}
            </div>
            <div class="flex justify-center gap-2">
              <button onClick={commitLink} disabled={!newLinkPartnerId()} class="rounded-md bg-primary px-3 py-1 text-xs font-medium text-primary-foreground hover:bg-primary/90 disabled:opacity-50">Add</button>
              <button onClick={() => { setAddingLink(false); setNewLinkPartnerId(""); }} class="rounded-md bg-secondary px-3 py-1 text-xs text-secondary-foreground hover:bg-secondary/80">Cancel</button>
            </div>
          </div>
        </Show>

        <Show when={!addingLink()}>
          <button onClick={() => setAddingLink(true)} class="flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground transition-colors">
            <MaterialIcon name="add" size="sm" />
            Add Link
          </button>
        </Show>
      </div>
    </div>
  );
}
