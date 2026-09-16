import { For, Show, createSignal } from "solid-js";
import type { ModRow } from "../../lib/types";
import { AlertTriangleIcon } from "../icons";
import { ModIcon } from "../ModIcon";
import {
  incompatibilityModalOpen,
  setIncompatibilityModalOpen,
  focusedIncompatibilityMod,
  draftIncompatibilities,
  rowMap,
  priorityParadoxDetected,
  setPairConflictEnabled,
  setPairWinner,
  incompatibilityFocusId,
} from "../../store";
import { Modal, ModalHeader } from "./modal-base";

interface IncompatibilitiesModalProps {
  onSave: () => Promise<void>;
}

export function IncompatibilitiesModal(props: IncompatibilitiesModalProps) {
  const [saving, setSaving] = createSignal(false);
  const [incompatSearch, setIncompatSearch] = createSignal("");

  const handleSave = async () => {
    if (saving() || priorityParadoxDetected()) return;
    setSaving(true);
    try {
      await props.onSave();
    } finally {
      setSaving(false);
    }
  };

  const incompatibilityExcluded = () => {
    const focusId = incompatibilityFocusId();
    const excluded = new Set<string>();
    if (!focusId) return excluded;
    excluded.add(focusId);

    const parentMap = new Map<string, string>();
    for (const row of rowMap().values()) {
      for (const alt of row.alternatives ?? []) {
        parentMap.set(alt.id, row.id);
      }
    }

    let current = focusId;
    while (parentMap.has(current)) {
      const parentId = parentMap.get(current)!;
      excluded.add(parentId);
      current = parentId;
    }

    const collectDescendants = (row: ModRow) => {
      for (const alt of row.alternatives ?? []) {
        excluded.add(alt.id);
        collectDescendants(alt);
      }
    };

    const focusedMod = rowMap().get(focusId);
    if (focusedMod) collectDescendants(focusedMod);

    return excluded;
  };

  return (
    <Show when={incompatibilityModalOpen()}>
      <Modal onClose={() => setIncompatibilityModalOpen(false)}>
        <ModalHeader title="Incompatibility Rules" description={`Define which mods conflict with "${focusedIncompatibilityMod()?.name ?? "selected"}"`} onClose={() => setIncompatibilityModalOpen(false)} />
        <div class="px-6 pt-4">
          <input
            type="text"
            placeholder="Search mods..."
            value={incompatSearch()}
            onInput={e => setIncompatSearch(e.currentTarget.value)}
            class="w-full rounded-md border border-border bg-input px-3 py-1.5 text-sm text-foreground placeholder:text-muted-foreground outline-none focus:ring-1 focus:ring-primary"
          />
        </div>
        <div class="flex-1 space-y-3 overflow-y-auto p-6">
          <Show when={priorityParadoxDetected()}>
            <div class="flex items-center gap-2 rounded-md border border-destructive/40 bg-destructive/10 p-3 text-sm text-destructive">
              <AlertTriangleIcon class="h-4 w-4 shrink-0" />
              Attention, you have created a priority paradox! Remove the conflicting rule to continue.
            </div>
          </Show>
          <For each={[...rowMap().values()].filter(row => !incompatibilityExcluded().has(row.id) && (!incompatSearch().trim() || row.name.toLowerCase().includes(incompatSearch().trim().toLowerCase())))}>
            {other => {
              const pair = () => draftIncompatibilities().find(rule =>
                (rule.winnerId === incompatibilityFocusId() && rule.loserId === other.id) ||
                (rule.winnerId === other.id && rule.loserId === incompatibilityFocusId())
              );
              const enabled = () => !!pair();
              const focusWins = () => pair()?.winnerId === incompatibilityFocusId();

              return (
                <div class={`rounded-md border p-3 transition-colors ${enabled() ? "border-border bg-background" : "border-border/50 bg-background/50"}`}>
                  <div class="flex items-center gap-3">
                    <input
                      type="checkbox"
                      checked={enabled()}
                      onChange={e => setPairConflictEnabled(incompatibilityFocusId()!, other.id, e.currentTarget.checked)}
                      class="h-4 w-4 shrink-0 rounded text-primary"
                    />
                    <Show
                      when={enabled()}
                      fallback={<span class="flex items-center gap-1.5 text-sm text-muted-foreground"><ModIcon row={other} />{other.name}</span>}
                    >
                      <div class="flex flex-1 flex-wrap items-center gap-2">
                        <button
                          onClick={() => setPairWinner(incompatibilityFocusId()!, other.id, incompatibilityFocusId()!)}
                          title={focusWins() ? "Currently wins - click to make it lose" : "Currently loses - click to make it win"}
                          class={`rounded-md px-2.5 py-0.5 text-sm font-medium transition-colors ${focusWins() ? "bg-green-500/15 text-green-500 ring-1 ring-green-500/30" : "bg-red-500/15 text-red-500 ring-1 ring-red-500/30"}`}
                        >
                          <span class="inline-flex items-center gap-1"><ModIcon row={focusedIncompatibilityMod() ?? undefined} />{focusedIncompatibilityMod()?.name}</span>
                        </button>

                        <span class="text-xs text-muted-foreground">vs</span>

                        <button
                          onClick={() => setPairWinner(incompatibilityFocusId()!, other.id, other.id)}
                          title={!focusWins() ? "Currently wins - click to make it lose" : "Currently loses - click to make it win"}
                          class={`rounded-md px-2.5 py-0.5 text-sm font-medium transition-colors ${!focusWins() ? "bg-green-500/15 text-green-500 ring-1 ring-green-500/30" : "bg-red-500/15 text-red-500 ring-1 ring-red-500/30"}`}
                        >
                          <span class="inline-flex items-center gap-1"><ModIcon row={other} />{other.name}</span>
                        </button>
                      </div>
                    </Show>
                  </div>
                </div>
              );
            }}
          </For>
        </div>
        <div class="flex justify-end gap-2 border-t border-border px-6 py-4">
          <button onClick={() => setIncompatibilityModalOpen(false)} class="rounded-md bg-secondary px-4 py-2 text-sm text-secondary-foreground hover:bg-secondary/80">Cancel</button>
          <button onClick={() => void handleSave()} disabled={priorityParadoxDetected() || saving()} class="rounded-md bg-primary px-4 py-2 text-sm font-medium text-primary-foreground hover:bg-primary/90 disabled:opacity-50">{saving() ? "Saving..." : "Save Rules"}</button>
        </div>
      </Modal>
    </Show>
  );
}
