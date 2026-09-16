/**
 * PinVersionDialog — the wiring behind the pin modal.
 *
 * It owns everything the modal must not know: which row was clicked, whether
 * that row is a pin or the dynamic entry under one, and the two Tauri
 * commands. The modal stays a pure view so it can be mounted with fixtures.
 */
import { Show } from "solid-js";
import { invoke } from "@tauri-apps/api/core";

import {
  mcWithSnapshots,
  minecraftVersions,
  parentIdByChildId,
  pinModalRowId,
  pinnedRowIds,
  rowMap,
  selectedMcVersion,
  selectedModListName,
  selectedModLoader,
  setPinModalRowId,
  showSnapshots,
} from "../store";
import { loadEditorSnapshot } from "../app/backend-loaders";
import { normalizeModLoader, type ModRow } from "../lib/types";
import { fetchPinReleases } from "../lib/pin-versions";
import { PinVersionModal } from "./modals/pin-version-modal";

type PinContext = {
  /** The Modrinth entry being pinned, when there is still one. */
  dynamic: ModRow | null;
  /** Name shown in the title. */
  modName: string;
  /** The pin already in place, if any — its backend mod_id. */
  pinnedModId?: string;
};

function pinContext(row: ModRow): PinContext {
  // Clicked on the pin itself: the release list belongs to the dynamic entry
  // parked under it, which is also the only thing that can be re-pinned.
  if (pinnedRowIds().has(row.id)) {
    const dynamic = (row.alternatives ?? []).find(alt => alt.kind === "modrinth") ?? null;
    return { dynamic, modName: dynamic?.name ?? row.name, pinnedModId: row.primaryModId };
  }

  const parentId = parentIdByChildId().get(row.id);
  const parent = parentId ? rowMap().get(parentId) : undefined;
  const pinnedModId = parent && pinnedRowIds().has(parent.id) ? parent.primaryModId : undefined;

  return { dynamic: row, modName: row.name, pinnedModId };
}

export function PinVersionDialog() {
  const row = () => {
    const id = pinModalRowId();
    return id ? rowMap().get(id) ?? null : null;
  };

  const close = () => setPinModalRowId(null);

  const refresh = async () => {
    const modlistName = selectedModListName();
    if (modlistName) await loadEditorSnapshot(modlistName);
  };

  return (
    <Show when={row()}>
      {current => {
        const context = pinContext(current());
        const loader = normalizeModLoader(selectedModLoader());
        const slug = context.dynamic?.modrinth_id ?? context.dynamic?.primaryModId ?? "";

        return (
          <PinVersionModal
            modName={context.modName}
            mcVersions={showSnapshots() ? mcWithSnapshots() : minecraftVersions()}
            initialMcVersion={selectedMcVersion()}
            loader={loader}
            pinnedLabel={context.pinnedModId}
            canPin={context.dynamic !== null}
            loadReleases={mcVersion =>
              slug ? fetchPinReleases(slug, mcVersion, loader) : Promise.resolve([])
            }
            onSave={async choice => {
              await invoke("pin_mod_version_command", {
                input: {
                  modlistName: selectedModListName(),
                  modId: context.dynamic!.primaryModId,
                  versionId: choice.versionId,
                  minecraftVersion: choice.mcVersion,
                  modLoader: loader,
                  removeDynamic: choice.removeDynamic,
                },
              });
              await refresh();
              close();
            }}
            onRemovePin={
              context.pinnedModId
                ? async () => {
                    await invoke("remove_pin_command", {
                      input: {
                        modlistName: selectedModListName(),
                        pinnedModId: context.pinnedModId,
                      },
                    });
                    await refresh();
                    close();
                  }
                : undefined
            }
            onClose={close}
          />
        );
      }}
    </Show>
  );
}
