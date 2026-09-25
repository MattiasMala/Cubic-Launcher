import { invoke } from "@tauri-apps/api/core";
import { For, Show, createMemo, createSignal, onMount } from "solid-js";
import { modListCards } from "../../store";
import { loadInstancesByModlist } from "../../lib/instances";
import {
  worldIdOf,
  type ShareOutcome,
  type WorldEntry,
  type WorldId,
  type WorldInstanceLink,
} from "../../lib/types";

/**
 * La tab «Worlds» dei Settings di una modlist (E4 fase 3).
 *
 * La forma è quella che ha dato Mattias: scelto un mondo, **tutte** le istanze
 * del launcher raggruppate per modlist, e si spunta quelle che devono vederlo.
 * Tutte e non solo quelle di questa modlist, perché da D99 un mondo si
 * condivide anche fra modlist diverse — mondo per mondo (D100), non collegando
 * le modlist come per i file condivisi.
 *
 * Due cose che questa schermata non deve nascondere:
 *
 * - **La prima spunta sposta byte.** Il mondo esce dall'istanza e va in
 *   `<root>/worlds/`. Non è l'interruttore innocuo di `servers.dat`, e la riga
 *   sopra l'elenco lo dice prima del clic, non dopo.
 * - **L'istanza di casa non si toglie.** Un mondo non ancora condiviso è di
 *   quell'istanza: la sua riga è accesa e ferma, perché spegnerla vorrebbe dire
 *   cancellare il mondo, e il backend lo rifiuta comunque.
 *
 * Quali mondi mostra, e come si sceglie quale, sono le decisioni che il prompt
 * lascia a Mattias: qui c'è la cosa più piccola che funziona — i mondi che
 * un'istanza di questa modlist può aprire, in fila sopra l'elenco.
 */

interface Props {
  modlist: string;
}

/** `test2 · 26.3-fabric` for a world that still lives in an instance, `null` once shared. */
function homeLabel(world: WorldId): string | null {
  return world.scope === "instance" ? `${world.modlistName} · ${world.instanceName}` : null;
}

/** Lo stesso id in forma di stringa, per confrontare e per selezionare. */
function keyOf(world: WorldId): string {
  return world.scope === "shared"
    ? `shared/${world.folderName}`
    : `${world.modlistName}/${world.instanceName}/${world.folderName}`;
}

export function WorldsPanel(props: Props) {
  const [worlds, setWorlds] = createSignal<WorldEntry[]>([]);
  const [instances, setInstances] = createSignal<Record<string, string[]>>({});
  const [selectedKey, setSelectedKey] = createSignal<string | null>(null);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  onMount(async () => {
    try {
      setWorlds(await invoke<WorldEntry[]>("list_worlds_command"));
      setInstances(await loadInstancesByModlist(modListCards().map(card => card.name).sort()));
    } catch (reason) {
      setError(String(reason));
    }
  });

  /**
   * I mondi che questa modlist vede: i suoi, e quelli condivisi che almeno una
   * sua istanza apre. Anche quelli nascosti dalla home — nascondere è una cosa
   * della home, non della condivisione.
   */
  const ours = createMemo(() =>
    worlds().filter(world =>
      world.scope === "instance"
        ? world.modlistName === props.modlist
        : world.instances.some(way => way.modlistName === props.modlist),
    ),
  );

  const selected = createMemo(() => {
    const list = ours();
    return list.find(world => keyOf(world) === selectedKey()) ?? list[0] ?? null;
  });

  const wayInto = (world: WorldEntry, modlistName: string, instanceName: string) =>
    world.instances.find(
      way => way.modlistName === modlistName && way.instanceName === instanceName,
    );

  const toggle = async (world: WorldEntry, modlistName: string, instanceName: string) => {
    if (busy()) return;
    setBusy(true);
    setError(null);
    try {
      const way: WorldInstanceLink | undefined = wayInto(world, modlistName, instanceName);
      if (way) {
        setWorlds(
          await invoke<WorldEntry[]>("unshare_world_from_instance_command", {
            modlistName: way.modlistName,
            instanceName: way.instanceName,
            folderName: way.folderName,
          }),
        );
      } else {
        const outcome = await invoke<ShareOutcome>("share_world_with_instance_command", {
          world: worldIdOf(world),
          targetModlistName: modlistName,
          targetInstanceName: instanceName,
        });
        setWorlds(outcome.worlds);
        // The first share moves and renames the world: follow it.
        setSelectedKey(keyOf(outcome.world));
      }
    } catch (reason) {
      setError(String(reason));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div class="flex flex-col gap-4" data-testid="worlds-panel">
      <div class="rounded-md border border-border bg-background px-3 py-2 text-xs text-muted-foreground">
        A shared world is one world that several instances open — of this mod list or of any
        other. Each instance reaches it through a link, so a game played in one continues in
        the others.
      </div>

      <Show when={error()}>
        <div
          class="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive"
          data-testid="worlds-error"
        >
          {error()}
        </div>
      </Show>

      <Show
        when={ours().length > 0}
        fallback={
          <p class="text-sm text-muted-foreground">
            No instance of <strong>{props.modlist}</strong> has a world yet. Play one once and it
            shows up here.
          </p>
        }
      >
        {/* Which world. */}
        <div class="flex flex-wrap gap-2" data-testid="worlds-picker">
          <For each={ours()}>
            {world => (
              <button
                class={`rounded-md border px-3 py-1.5 text-left text-xs transition-colors ${
                  selected() && keyOf(selected()!) === keyOf(world)
                    ? "border-primary bg-primary/10 text-primary"
                    : "border-border hover:bg-accent"
                }`}
                onClick={() => setSelectedKey(keyOf(world))}
              >
                <span class="block text-sm font-medium">{world.levelName}</span>
                <span class="block text-[11px] text-muted-foreground">
                  {world.scope === "shared"
                    ? `shared · ${world.folderName}`
                    : `${world.instanceName} · ${world.folderName}`}
                </span>
              </button>
            )}
          </For>
        </div>

        <Show when={selected()}>
          {world => (
            <>
              {/* The line that says what the first switch does to the disk. */}
              <Show
                when={world().scope === "instance"}
                fallback={
                  <p class="text-xs text-muted-foreground" data-testid="worlds-where">
                    This world lives in the launcher's shared worlds folder. Switching an instance
                    off removes only its way in; the world stays.
                  </p>
                }
              >
                <p
                  class="rounded-md border border-warning/40 bg-warning/10 px-3 py-2 text-xs text-warning"
                  data-testid="worlds-move-warning"
                >
                  Sharing moves this world out of <strong>{homeLabel(world())}</strong> into
                  the launcher's shared worlds folder. Every instance then opens it through a
                  link — that one included.
                </p>
              </Show>

              <div class="flex flex-col gap-3">
                <For each={Object.keys(instances())}>
                  {modlistName => (
                    <div class="rounded-lg border border-border bg-card px-4 py-3">
                      <p class="text-sm font-medium">{modlistName}</p>
                      <div class="mt-2 flex flex-col gap-1">
                        <For
                          each={instances()[modlistName]}
                          fallback={
                            <p class="text-xs text-muted-foreground">No instance yet.</p>
                          }
                        >
                          {instanceName => {
                            const on = () => Boolean(wayInto(world(), modlistName, instanceName));
                            // The home of a world not yet shared: on, and fixed.
                            const home = () => homeLabel(world()) === `${modlistName} · ${instanceName}`;
                            return (
                              <div class="flex items-center gap-3 py-1">
                                <span class="min-w-0 flex-1 text-sm">
                                  {instanceName}
                                  <Show when={home()}>
                                    <span class="ml-2 text-xs text-muted-foreground">
                                      — this world lives here
                                    </span>
                                  </Show>
                                </span>
                                <button
                                  role="switch"
                                  aria-checked={on()}
                                  aria-label={`${modlistName} · ${instanceName} sees ${world().levelName}`}
                                  disabled={busy() || home()}
                                  data-testid={`worlds-switch-${modlistName}-${instanceName}`}
                                  onClick={() => void toggle(world(), modlistName, instanceName)}
                                  class={`h-6 w-11 shrink-0 rounded-full border transition-colors disabled:opacity-60 ${
                                    on() ? "border-primary bg-primary" : "border-border bg-muted"
                                  }`}
                                >
                                  <span
                                    class={`block h-4 w-4 rounded-full bg-background transition-transform ${
                                      on() ? "translate-x-6" : "translate-x-1"
                                    }`}
                                  />
                                </button>
                              </div>
                            );
                          }}
                        </For>
                      </div>
                    </div>
                  )}
                </For>
              </div>
            </>
          )}
        </Show>
      </Show>
    </div>
  );
}
