import { Show } from "solid-js";
import { advancedPanelMod, advancedPanelModId, setAdvancedPanelModId } from "../store";
import { XIcon } from "./icons";
import { ModIcon } from "./ModIcon";
import { VersionRulesSection } from "./advanced-mod-panel/VersionRulesSection";
import { TagsSection } from "./advanced-mod-panel/TagsSection";
import { LinksSection } from "./advanced-mod-panel/LinksSection";
import { RelationshipsSection } from "./advanced-mod-panel/RelationshipsSection";
import { IncompatibilitiesSection } from "./advanced-mod-panel/IncompatibilitiesSection";
import { CustomConfigsSection } from "./advanced-mod-panel/CustomConfigsSection";

export function AdvancedModPanel(props: { onDelete?: (modId: string) => void }) {
  const row = () => advancedPanelMod();
  const modId = () => advancedPanelModId()!;

  return (
    <Show when={row()}>
      <div
        class="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-black/60 px-4 py-8 backdrop-blur-sm"
        onClick={e => { if (e.target === e.currentTarget) setAdvancedPanelModId(null); }}
      >
        <div class="flex w-full max-w-2xl flex-col overflow-hidden rounded-lg border border-border bg-card shadow-xl">
          <div class="flex items-center justify-between border-b border-border px-6 py-4 shrink-0">
            <div>
              <h2 class="text-lg font-semibold text-foreground">Advanced</h2>
              <p class="flex items-center gap-1.5 text-sm text-muted-foreground truncate max-w-md"><ModIcon row={row()!} />{row()!.name}</p>
            </div>
            <button
              onClick={() => setAdvancedPanelModId(null)}
              class="flex h-8 w-8 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
            >
              <XIcon class="h-4 w-4" />
            </button>
          </div>

          <div class="flex-1 overflow-y-auto max-h-[70vh] divide-y divide-border">
            <VersionRulesSection modId={modId()} />
            <TagsSection modId={modId()} />
            <LinksSection modId={modId()} row={row()!} />
            <RelationshipsSection modId={modId()} row={row()!} />
            <IncompatibilitiesSection modId={modId()} row={row()!} />
          </div>

          <CustomConfigsSection modId={modId()} />

          <Show when={props.onDelete}>
            <div class="border-t border-border p-4 shrink-0">
              <button
                onClick={() => { props.onDelete!(modId()); setAdvancedPanelModId(null); }}
                class="flex w-full items-center justify-center gap-1.5 rounded-md border border-destructive/40 bg-destructive/10 px-4 py-2 text-sm font-medium text-destructive hover:bg-destructive/20 transition-colors"
              >
                <XIcon class="h-4 w-4" />
                Delete Mod
              </button>
            </div>
          </Show>
        </div>
      </div>
    </Show>
  );
}
