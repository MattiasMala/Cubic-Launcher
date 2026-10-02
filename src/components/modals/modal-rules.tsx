import { Show } from "solid-js";
import {
  functionalGroupModalOpen,
  setFunctionalGroupModalOpen,
  newFunctionalGroupName,
  setNewFunctionalGroupName,
  functionalGroupTone,
  setFunctionalGroupTone,
  createFunctionalGroup,
  toneToHue,
  huePreviewColor,
} from "../../store";
import { Modal, ModalHeader } from "./modal-base";

export { LinkModal, LinksOverviewModal } from "./link-modals";
export { IncompatibilitiesModal } from "./incompatibilities-modal";
export { AlternativesPanel } from "./alternatives-panel";

export function FunctionalGroupModal() {
  const currentHue = () => toneToHue(functionalGroupTone());

  return (
    <Show when={functionalGroupModalOpen()}>
      <Modal onClose={() => setFunctionalGroupModalOpen(false)}>
        <ModalHeader title="Create Tag" description="Tags let you label and filter mods by category." onClose={() => setFunctionalGroupModalOpen(false)} />
        <div class="space-y-4 p-6">
          <div>
            <label class="mb-1.5 block text-sm font-medium text-foreground">Tag Name</label>
            <input
              type="text"
              value={newFunctionalGroupName()}
              onInput={e => setNewFunctionalGroupName(e.currentTarget.value)}
              placeholder="Performance Core"
              class="w-full rounded-md border border-input bg-input px-3 py-2 text-sm text-foreground placeholder:text-muted-foreground focus:outline-none focus:ring-1 focus:ring-ring"
              autofocus
            />
          </div>
          <div>
            <label class="mb-1.5 block text-sm font-medium text-foreground">Tag Color</label>
            <div class="flex items-center gap-3">
              <div class="h-7 w-7 shrink-0 rounded-full border border-border" style={`background-color: ${huePreviewColor(currentHue())}`} />
              <input
                type="range"
                min="0"
                max="360"
                value={currentHue()}
                onInput={e => setFunctionalGroupTone(e.currentTarget.value)}
                class="h-3 flex-1 cursor-pointer appearance-none rounded-full"
                style="background: linear-gradient(to right, hsl(0,70%,55%), hsl(30,70%,55%), hsl(60,70%,55%), hsl(90,70%,55%), hsl(120,70%,55%), hsl(150,70%,55%), hsl(180,70%,55%), hsl(210,70%,55%), hsl(240,70%,55%), hsl(270,70%,55%), hsl(300,70%,55%), hsl(330,70%,55%), hsl(360,70%,55%));"
              />
            </div>
          </div>
        </div>
        <div class="flex justify-end gap-2 border-t border-border px-6 py-4">
          <button onClick={() => setFunctionalGroupModalOpen(false)} class="rounded-md bg-secondary px-4 py-2 text-sm text-secondary-foreground hover:bg-secondary/80">Cancel</button>
          <button onClick={createFunctionalGroup} disabled={!newFunctionalGroupName().trim()} class="rounded-md bg-primary px-4 py-2 text-sm font-medium text-primary-foreground hover:bg-primary/90 disabled:opacity-50">Create</button>
        </div>
      </Modal>
    </Show>
  );
}
