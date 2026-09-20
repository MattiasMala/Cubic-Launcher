// The landing page: the worlds to jump back into, then the mod list library.
//
// Three decisions worth stating once, because all three are visible on this
// machine's real data:
//
// - **The two real worlds are both called `New World`, both in creative,
//   both without an icon.** So the name cannot carry the card: what tells
//   them apart is the mod list and the instance, and that line is given the
//   same weight as the name. The icon tile is generated from the triple, so
//   two worlds with the same name still look different at a glance.
// - **"No icon" is the normal case, not the edge case.** Minecraft writes
//   `icon.png` when the player leaves a world through the menu, and neither
//   real world has one. The fallback is designed — a tinted tile with the
//   mod list's own badge on it — rather than a grey placeholder.
// - **Hiding a world means "not now".** There is no show-hidden switch here
//   on purpose (D65): the backend un-hides a world by itself the next time
//   its `LastPlayed` moves, so a hidden card comes back after the next
//   session in it.

import { For, Show, createMemo, createResource, createSignal, onCleanup } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import {
  librarySearch, modListCards, pushUiError, setCreateModlistModalOpen, setLibrarySearch,
} from "../store";
import type { ModListCard, WorldEntry, WorldGameMode } from "../lib/types";
import { MaterialIcon } from "./icons";

interface HomeViewProps {
  /** Launch this world's instance and open the world directly. */
  onPlayWorld: (world: WorldEntry) => void;
  /** Launch this world's instance the ordinary way, stopping at the menu. */
  onPlayModlistOf: (world: WorldEntry) => void;
  /** Show the mod list's editor and launch panel, as its rail icon does. */
  onOpenModlist: (modlistName: string) => void;
}

const GAME_MODE_LABELS: Record<WorldGameMode, string> = {
  survival: "Survival",
  creative: "Creative",
  adventure: "Adventure",
  spectator: "Spectator",
  unknown: "Unknown mode",
};

/** "3 hours ago", down to "just now"; months once the days stop being useful. */
function playedAgo(lastPlayedMs: number): string {
  if (lastPlayedMs <= 0) return "never played";
  const seconds = Math.round((Date.now() - lastPlayedMs) / 1000);
  if (seconds < 0) return "just now";
  // Each pair is "divide by this, and you are now counting these": the unit
  // named is the one you arrive at, not the one you left.
  const scale: [number, string][] = [
    [60, "minute"], [60, "hour"], [24, "day"], [7, "week"], [4.345, "month"], [12, "year"],
  ];
  let value = seconds;
  let unit = "second";
  for (const [step, nextUnit] of scale) {
    if (value < step) break;
    value = value / step;
    unit = nextUnit;
  }
  const rounded = Math.floor(value);
  if (unit === "second" && rounded < 45) return "just now";
  return `${rounded} ${unit}${rounded === 1 ? "" : "s"} ago`;
}

/**
 * A stable hue for a world, taken from its whole triple.
 *
 * It is what keeps two worlds called `New World` from looking identical
 * while neither of them has an icon.
 */
function worldHue(world: WorldEntry): number {
  const key = `${world.modlistName}/${world.instanceName}/${world.folderName}`;
  let hash = 0;
  for (let index = 0; index < key.length; index += 1) {
    hash = (Math.imul(hash, 31) + key.charCodeAt(index)) >>> 0;
  }
  // The murmur3 finalizer, because the plain rolling hash is not enough here:
  // the two real triples differ late and by little, and `hash % 360` put them
  // five degrees apart — two cards that already share a name would also have
  // shared a colour.
  hash ^= hash >>> 15;
  hash = Math.imul(hash, 2246822507) >>> 0;
  hash ^= hash >>> 13;
  hash = Math.imul(hash, 3266489909) >>> 0;
  hash ^= hash >>> 16;
  return hash % 360;
}

/** The world tile: the real `icon.png` when there is one, the generated one otherwise. */
function WorldTile(props: { world: WorldEntry; modlist: ModListCard | undefined }) {
  const [iconSource] = createResource(
    () => props.world.iconPath,
    async (path) => await invoke<string>("read_image_as_data_url_command", { path }),
  );
  const hue = createMemo(() => worldHue(props.world));

  return (
    <div class="relative w-16 h-16 shrink-0">
      <Show
        when={iconSource()}
        fallback={
          <div
            class="w-16 h-16 rounded-lg flex items-center justify-center border border-borderColor/60"
            style={{
              background: `linear-gradient(135deg, hsl(${hue()} 45% 32%), hsl(${(hue() + 40) % 360} 40% 18%))`,
            }}
            title="This world has no icon yet — Minecraft writes one when you leave the world through the menu"
          >
            <MaterialIcon name="public" size="xl" class="text-white/80" />
          </div>
        }
      >
        {source => <img src={source()} alt="" class="w-16 h-16 rounded-lg object-cover" />}
      </Show>

      {/* Whose mod list this world belongs to, on the tile itself. */}
      <span
        class="absolute -bottom-1 -right-1 w-6 h-6 rounded-md overflow-hidden border border-bgDark bg-muted flex items-center justify-center"
        title={props.world.modlistName}
      >
        <Show
          when={props.modlist?.iconImage}
          fallback={
            <span class="text-[9px] font-bold text-white">
              {(props.modlist?.displayName || props.world.modlistName).trim().slice(0, 2).toUpperCase()}
            </span>
          }
        >
          <img src={props.modlist!.iconImage} alt="" class="w-6 h-6 object-cover" />
        </Show>
      </span>
    </div>
  );
}

function WorldCard(props: {
  world: WorldEntry;
  modlist: ModListCard | undefined;
  onPlay: () => void;
  onPlayModlist: () => void;
  onOpenModlist: () => void;
  onOpenFolder: () => void;
  onHide: () => void;
}) {
  const [menuOpen, setMenuOpen] = createSignal(false);

  const closeOnOutside = (event: MouseEvent) => {
    const target = event.target as HTMLElement | null;
    if (!target?.closest("[data-world-menu]")) setMenuOpen(false);
  };
  const closeOnEscape = (event: KeyboardEvent) => {
    if (event.key === "Escape") setMenuOpen(false);
  };
  document.addEventListener("click", closeOnOutside);
  document.addEventListener("keydown", closeOnEscape);
  onCleanup(() => {
    document.removeEventListener("click", closeOnOutside);
    document.removeEventListener("keydown", closeOnEscape);
  });

  const run = (action: () => void) => {
    setMenuOpen(false);
    action();
  };

  return (
    <div class="flex items-center gap-4 p-4 rounded-xl bg-bgPanel border border-borderColor hover:border-primary/60 transition-colors">
      <WorldTile world={props.world} modlist={props.modlist} />

      <div class="min-w-0 flex-1">
        <div class="text-textMain font-semibold truncate" title={props.world.levelName}>
          {props.world.levelName}
        </div>
        {/* The line that tells two worlds of the same name apart. */}
        <div class="flex items-center gap-1.5 text-sm text-textMain/90 truncate">
          <MaterialIcon name="folder_managed" size="sm" class="text-textMuted" />
          <span class="truncate" title={`${props.world.modlistName} — ${props.world.instanceName}`}>
            {props.modlist?.displayName || props.world.modlistName}
            <span class="text-textMuted"> · {props.world.instanceName}</span>
          </span>
        </div>
        <div class="text-xs text-textMuted truncate">
          Singleplayer · {GAME_MODE_LABELS[props.world.gameMode]} · played {playedAgo(props.world.lastPlayedMs)}
        </div>
      </div>

      <button
        class="px-4 h-9 rounded-lg bg-primary hover:bg-brandPurpleHover text-white text-sm font-medium flex items-center gap-1.5 shrink-0"
        onClick={props.onPlay}
        // The card must not promise what the instance may not do: the
        // backend passes `--quickPlaySingleplayer` only when that client's
        // own manifest declares it, and otherwise the game opens at the menu.
        title="Play — opens this world directly on Minecraft 1.20 and newer; older versions open at the menu"
      >
        <MaterialIcon name="play_arrow" size="sm" />
        Play
      </button>

      <div class="relative shrink-0" data-world-menu>
        <button
          class="w-9 h-9 rounded-lg border border-borderColor text-textMuted hover:text-textMain hover:bg-bgHover flex items-center justify-center"
          aria-label={`More actions for ${props.world.levelName}`}
          onClick={() => setMenuOpen(open => !open)}
        >
          <MaterialIcon name="more_vert" size="sm" />
        </button>

        <Show when={menuOpen()}>
          <div class="absolute right-0 top-full mt-1 w-56 rounded-lg border border-borderColor bg-popover shadow-xl py-1 z-30">
            <button
              class="w-full px-3 py-2 text-left text-sm text-textMain hover:bg-bgHover flex items-center gap-2"
              onClick={() => run(props.onOpenFolder)}
            >
              <MaterialIcon name="folder_open" size="sm" /> Open folder
            </button>
            <button
              class="w-full px-3 py-2 text-left text-sm text-textMain hover:bg-bgHover flex items-center gap-2"
              onClick={() => run(props.onPlayModlist)}
            >
              <MaterialIcon name="play_circle" size="sm" /> Play the mod list
            </button>
            <button
              class="w-full px-3 py-2 text-left text-sm text-textMain hover:bg-bgHover flex items-center gap-2"
              onClick={() => run(props.onOpenModlist)}
            >
              <MaterialIcon name="tune" size="sm" /> Go to the mod list
            </button>
            <div class="my-1 border-t border-borderColor" />
            <button
              class="w-full px-3 py-2 text-left text-sm text-textMain hover:bg-bgHover flex items-center gap-2"
              onClick={() => run(props.onHide)}
              title="It comes back on its own the next time you play it"
            >
              <MaterialIcon name="visibility_off" size="sm" /> Hide from Jump in
            </button>
          </div>
        </Show>
      </div>
    </div>
  );
}

export function HomeView(props: HomeViewProps) {
  const [worlds, { refetch }] = createResource(async () => {
    try {
      return await invoke<WorldEntry[]>("list_worlds_command");
    } catch (error) {
      pushUiError({
        title: "Worlds could not be read",
        message: "The saves of your instances could not be listed.",
        detail: String(error),
        severity: "warning",
        scope: "launch",
      });
      return [] as WorldEntry[];
    }
  });

  /** Hidden worlds are not shown at all: the un-hiding is playing them (D65). */
  const visibleWorlds = createMemo(() => (worlds() ?? []).filter(world => !world.hidden));

  const filteredModlists = createMemo(() => {
    const needle = librarySearch().trim().toLowerCase();
    if (!needle) return modListCards();
    return modListCards().filter(card =>
      (card.displayName || card.name).toLowerCase().includes(needle) ||
      card.name.toLowerCase().includes(needle),
    );
  });

  const hideWorld = async (world: WorldEntry) => {
    try {
      await invoke("set_world_hidden_command", {
        modlistName: world.modlistName,
        instanceName: world.instanceName,
        folderName: world.folderName,
        hidden: true,
      });
      void refetch();
    } catch (error) {
      pushUiError({
        title: "The world could not be hidden",
        message: `'${world.levelName}' is still in Jump in.`,
        detail: String(error),
        severity: "error",
        scope: "launch",
      });
    }
  };

  const openWorldFolder = async (world: WorldEntry) => {
    try {
      await invoke("open_world_folder_command", {
        modlistName: world.modlistName,
        instanceName: world.instanceName,
        folderName: world.folderName,
      });
    } catch (error) {
      pushUiError({
        title: "The folder could not be opened",
        message: `'${world.levelName}' could not be shown in the file manager.`,
        detail: String(error),
        severity: "error",
        scope: "launch",
      });
    }
  };

  return (
    <div class="flex-1 min-h-0 overflow-y-auto bg-bgDark">
      <div class="max-w-5xl mx-auto px-8 py-8 flex flex-col gap-10">
        {/* ── Jump in ─────────────────────────────────────────────────── */}
        <section>
          <h2 class="text-lg font-semibold text-textMain mb-1">Jump in</h2>
          <p class="text-sm text-textMuted mb-4">
            Your most recent worlds. Play opens the world itself, without going through the menu.
          </p>

          <Show
            when={visibleWorlds().length > 0}
            fallback={
              <div class="rounded-xl border border-dashed border-borderColor p-6 text-sm text-textMuted">
                <Show
                  when={!worlds.loading}
                  fallback="Reading your worlds…"
                >
                  Nothing to jump into yet. Play a world once and it shows up here.
                </Show>
              </div>
            }
          >
            <div class="flex flex-col gap-3">
              <For each={visibleWorlds()}>
                {world => (
                  <WorldCard
                    world={world}
                    modlist={modListCards().find(card => card.name === world.modlistName)}
                    onPlay={() => props.onPlayWorld(world)}
                    onPlayModlist={() => props.onPlayModlistOf(world)}
                    onOpenModlist={() => props.onOpenModlist(world.modlistName)}
                    onOpenFolder={() => void openWorldFolder(world)}
                    onHide={() => void hideWorld(world)}
                  />
                )}
              </For>
            </div>
          </Show>
        </section>

        {/* ── Library ─────────────────────────────────────────────────── */}
        <section>
          <div class="flex items-center gap-3 mb-4">
            <h2 class="text-lg font-semibold text-textMain mr-auto">Library</h2>

            <div class="relative">
              <span class="absolute left-2.5 top-1/2 -translate-y-1/2 text-textMuted pointer-events-none">
                <MaterialIcon name="search" size="sm" />
              </span>
              <input
                class="w-60 h-9 pl-9 pr-3 rounded-lg bg-bgPanel border border-borderColor text-sm text-textMain placeholder:text-textMuted focus:outline-none focus:border-primary"
                placeholder="Search mod lists"
                value={librarySearch()}
                onInput={event => setLibrarySearch(event.currentTarget.value)}
              />
            </div>

            <button
              class="h-9 px-4 rounded-lg bg-primary hover:bg-brandPurpleHover text-white text-sm font-medium flex items-center gap-1.5"
              onClick={() => setCreateModlistModalOpen(true)}
            >
              <MaterialIcon name="add" size="sm" /> New Modlist
            </button>
          </div>

          <Show
            when={filteredModlists().length > 0}
            fallback={
              <div class="rounded-xl border border-dashed border-borderColor p-6 text-sm text-textMuted">
                <Show
                  when={librarySearch().trim()}
                  fallback="No mod lists yet. 'New Modlist' makes the first one."
                >
                  No mod list matches “{librarySearch().trim()}”.
                </Show>
              </div>
            }
          >
            <div class="grid grid-cols-2 lg:grid-cols-3 gap-3">
              <For each={filteredModlists()}>
                {card => (
                  <button
                    class="flex items-center gap-3 p-3 rounded-xl bg-bgPanel border border-borderColor hover:border-primary/60 text-left transition-colors"
                    onClick={() => props.onOpenModlist(card.name)}
                  >
                    <Show
                      when={card.iconImage}
                      fallback={
                        <span class="w-12 h-12 rounded-lg bg-muted flex items-center justify-center text-white text-xs font-bold shrink-0">
                          {(card.displayName || card.name).slice(0, 3).toUpperCase()}
                        </span>
                      }
                    >
                      <img src={card.iconImage} alt="" class="w-12 h-12 rounded-lg object-cover shrink-0" />
                    </Show>
                    <span class="min-w-0">
                      <span class="block text-sm font-medium text-textMain truncate">
                        {card.displayName || card.name}
                      </span>
                      <span class="block text-xs text-textMuted truncate">
                        <Show when={card.modLoader || card.mcVersion} fallback="No target picked yet">
                          {[card.modLoader, card.mcVersion].filter(Boolean).join(" · ")}
                        </Show>
                      </span>
                    </span>
                  </button>
                )}
              </For>
            </div>
          </Show>
        </section>
      </div>
    </div>
  );
}
