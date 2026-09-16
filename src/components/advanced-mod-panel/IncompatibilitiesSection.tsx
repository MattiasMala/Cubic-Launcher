import { For, Show, createSignal } from "solid-js";
import { savedIncompatibilities, setSavedIncompatibilities, rowMap } from "../../store";
import type { ModRow } from "../../lib/types";
import { MaterialIcon, XIcon } from "../icons";
import { ModIcon } from "../ModIcon";
import { SectionHeader } from "./shared";

export function IncompatibilitiesSection(props: { modId: string; row: ModRow }) {
  const [addingIncompat, setAddingIncompat] = createSignal(false);
  const [newIncompatPartnerId, setNewIncompatPartnerId] = createSignal("");
  const [newIncompatMeWins, setNewIncompatMeWins] = createSignal(true);

  const myIncompats = () => {
    return savedIncompatibilities()
      .filter(rule => rule.winnerId === props.modId || rule.loserId === props.modId)
      .map(rule => ({
        otherId: rule.winnerId === props.modId ? rule.loserId : rule.winnerId,
        meWins: rule.winnerId === props.modId,
      }));
  };

  const availableIncompatPartners = () => {
    const existing = new Set(myIncompats().map(pair => pair.otherId));
    return [...rowMap().entries()]
      .filter(([id]) => id !== props.modId && !existing.has(id))
      .map(([id, row]) => ({ id, name: row.name }));
  };

  const commitIncompat = () => {
    const partner = newIncompatPartnerId();
    if (!partner) return;
    const winnerId = newIncompatMeWins() ? props.modId : partner;
    const loserId = newIncompatMeWins() ? partner : props.modId;
    setSavedIncompatibilities(current => [...current, { winnerId, loserId }]);
    setAddingIncompat(false);
    setNewIncompatPartnerId("");
    setNewIncompatMeWins(true);
  };

  const swapIncompat = (otherId: string) => {
    setSavedIncompatibilities(current => current.map(rule => {
      if (rule.winnerId === props.modId && rule.loserId === otherId) return { winnerId: otherId, loserId: props.modId };
      if (rule.winnerId === otherId && rule.loserId === props.modId) return { winnerId: props.modId, loserId: otherId };
      return rule;
    }));
  };

  const removeIncompat = (otherId: string) => {
    setSavedIncompatibilities(current => current.filter(rule =>
      !((rule.winnerId === props.modId && rule.loserId === otherId) || (rule.winnerId === otherId && rule.loserId === props.modId))
    ));
  };

  return (
    <div>
      <SectionHeader title="Incompatibilities" />
      <div class="p-4 space-y-2">
        <For each={myIncompats()}>
          {item => {
            const otherName = () => rowMap().get(item.otherId)?.name ?? item.otherId;
            return (
              <div class="flex items-center gap-2 rounded-md border border-border bg-background p-2">
                <span class="flex items-center gap-1 text-sm font-medium text-foreground truncate min-w-0 flex-1 justify-end"><ModIcon row={props.row} />{props.row.name}</span>
                <button
                  onClick={() => swapIncompat(item.otherId)}
                  class="shrink-0 rounded px-1.5 py-0.5 text-xs font-bold text-primary hover:bg-primary/20 transition-colors"
                  title={item.meWins ? `${props.row.name} wins - click to swap` : `${otherName()} wins - click to swap`}
                >
                  {item.meWins ? ">" : "<"}
                </button>
                <span class="flex items-center gap-1 text-sm font-medium text-foreground truncate min-w-0 flex-1"><ModIcon row={rowMap().get(item.otherId)} name={otherName()} />{otherName()}</span>
                <button
                  onClick={() => removeIncompat(item.otherId)}
                  class="shrink-0 flex h-6 w-6 items-center justify-center rounded text-muted-foreground hover:bg-destructive/10 hover:text-destructive transition-colors"
                >
                  <XIcon class="h-3.5 w-3.5" />
                </button>
              </div>
            );
          }}
        </For>
        <Show when={myIncompats().length === 0 && !addingIncompat()}>
          <span class="text-xs text-muted-foreground">No incompatibilities defined.</span>
        </Show>

        <Show when={addingIncompat()}>
          <div class="rounded-md border border-primary/30 bg-primary/5 p-3 space-y-2 overflow-hidden">
            <div class="flex items-center gap-2 min-w-0">
              <span class="text-sm font-medium text-foreground truncate shrink-0" style="max-width: 35%">{props.row.name}</span>
              <button
                onClick={() => setNewIncompatMeWins(value => !value)}
                class="shrink-0 rounded px-1.5 py-0.5 text-xs font-bold text-primary hover:bg-primary/20 transition-colors"
                title="Click to swap direction"
              >
                {newIncompatMeWins() ? ">" : "<"}
              </button>
              {(() => {
                const [incompatSearch, setIncompatSearch] = createSignal("");
                const filtered = () => {
                  const query = incompatSearch().trim().toLowerCase();
                  return query ? availableIncompatPartners().filter(partner => partner.name.toLowerCase().includes(query)) : availableIncompatPartners();
                };
                return (
                  <div class="min-w-0 flex-1 relative">
                    <input type="text" placeholder="Search mod..." value={incompatSearch()} onInput={e => { setIncompatSearch(e.currentTarget.value); setNewIncompatPartnerId(""); }} class="w-full rounded border border-border bg-input px-2 py-1 text-xs text-foreground placeholder:text-muted-foreground outline-none" />
                    <Show when={incompatSearch().trim() && filtered().length > 0}>
                      <div class="absolute left-0 right-0 top-full mt-1 z-20 max-h-32 overflow-y-auto rounded border border-border bg-card shadow-lg">
                        <For each={filtered()}>{partner => <button onClick={() => { setNewIncompatPartnerId(partner.id); setIncompatSearch(partner.name); }} class="flex w-full items-center gap-1.5 px-2 py-1 text-xs text-foreground hover:bg-muted/50 text-left"><ModIcon row={rowMap().get(partner.id)} name={partner.name} />{partner.name}</button>}</For>
                      </div>
                    </Show>
                  </div>
                );
              })()}
            </div>
            <div class="flex justify-center gap-2">
              <button onClick={commitIncompat} disabled={!newIncompatPartnerId()} class="rounded-md bg-primary px-3 py-1 text-xs font-medium text-primary-foreground hover:bg-primary/90 disabled:opacity-50">Add</button>
              <button onClick={() => { setAddingIncompat(false); setNewIncompatPartnerId(""); }} class="rounded-md bg-secondary px-3 py-1 text-xs text-secondary-foreground hover:bg-secondary/80">Cancel</button>
            </div>
          </div>
        </Show>

        <Show when={!addingIncompat()}>
          <button onClick={() => setAddingIncompat(true)} class="flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground transition-colors">
            <MaterialIcon name="add" size="sm" />
            Add Incompatibility
          </button>
        </Show>
      </div>
    </div>
  );
}
