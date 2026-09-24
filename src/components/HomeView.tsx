// The landing page: the worlds to jump back into, then the mod list library.
//
// Five decisions worth stating once, because all five are visible on this
// machine's real data:
//
// - **Three of the real worlds are called `New World`.** So the name cannot
//   carry the card: what tells them apart is the mod list and the instance,
//   and that line is given the same weight as the name. When two visible
//   worlds still share a name, the folder name is shown as well — only then,
//   because on a screen where every card carries one it stops meaning
//   anything. The icon tile is generated from the id, so two worlds with the
//   same name still look different at a glance.
// - **"No icon" is the normal case, not the edge case.** Minecraft writes
//   `icon.png` when the player leaves a world through the menu, and neither
//   real world has one. The fallback is designed — a tinted tile with the
//   mod list's own badge on it — rather than a grey placeholder.
// - **Hiding a world means "not now".** There is no show-hidden switch here
//   on purpose (D65): the backend un-hides a world by itself the next time
//   its `LastPlayed` moves, so a hidden card comes back after the next
//   session in it.
// - **A shared world is one card, not one per instance** (D95). It carries
//   the instances that can open it, and Play asks which one — but only when
//   there is something to ask: a world with a single way in gets no question,
//   because a question with one answer is a click the user did not need.
// - **No instance at all is a state that gets drawn** (E4 phase 1): a shared
//   world every instance has been taken off still exists, still holds its
//   bytes, and the card says so instead of offering a Play that cannot work.

import { For, Show, createMemo, createResource, createSignal, onCleanup } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import {
  librarySearch, modListCards, pushUiError, setCreateModlistModalOpen, setLibrarySearch,
} from "../store";
import type { ModListCard, WorldEntry, WorldGameMode, WorldInstanceLink } from "../lib/types";
import { MaterialIcon } from "./icons";

interface HomeViewProps {
  /** Launch this world through the chosen instance and open the world directly. */
  onPlayWorld: (world: WorldEntry, through: WorldInstanceLink) => void;
  /** Launch the chosen instance the ordinary way, stopping at the menu. */
  onPlayModlistOf: (world: WorldEntry, through: WorldInstanceLink) => void;
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
 * The id of a world as one string: mod list, instance or `worlds` for a
 * shared one, and folder.
 *
 * It keys the open ⋮ menu and seeds the tile's hue, and it has to be the
 * whole id: three of the real worlds are called `New World`.
 */
function worldKey(world: WorldEntry): string {
  return `${world.modlistName}/${world.instanceName ?? "worlds"}/${world.folderName}`;
}

/**
 * A stable hue for a world, taken from its whole id.
 *
 * It is what keeps two worlds called `New World` from looking identical
 * while neither of them has an icon.
 */
function worldHue(world: WorldEntry): number {
  const key = worldKey(world);
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
  /** Show the folder name too: another visible world carries the same name. */
  showFolderName: boolean;
  /** Every instance of this world's mod list, for the "Share with" list. */
  modlistInstances: string[];
  onPlay: (through: WorldInstanceLink) => void;
  onPlayModlist: (through: WorldInstanceLink) => void;
  onOpenModlist: () => void;
  onOpenFolder: () => void;
  onHide: () => void;
  onShareWith: (instanceName: string) => void;
  onUnshareFrom: (through: WorldInstanceLink) => void;
  /** Whether this card's ⋮ menu is the open one; at most one ever is. */
  menuOpen: boolean;
  onToggleMenu: () => void;
  onCloseMenu: () => void;
}) {
  const run = (action: () => void) => {
    props.onCloseMenu();
    action();
  };

  const ways = () => props.world.instances;
  const isShared = () => props.world.instanceName === null;

  /**
   * Which instance the Play is waiting on, or `null`.
   *
   * D95: a shared world does not know which instance to launch with, so it
   * has to ask. One way in is not a question — it is answered before it is
   * asked, and the click goes straight through.
   */
  const [asking, setAsking] = createSignal<"world" | "modlist" | null>(null);
  const start = (mode: "world" | "modlist") => {
    const list = ways();
    if (list.length === 0) return;
    if (list.length === 1) {
      setAsking(null);
      if (mode === "world") props.onPlay(list[0]);
      else props.onPlayModlist(list[0]);
      return;
    }
    setAsking(current => (current === mode ? null : mode));
  };
  const answer = (through: WorldInstanceLink) => {
    const mode = asking();
    setAsking(null);
    if (mode === "world") props.onPlay(through);
    else if (mode === "modlist") props.onPlayModlist(through);
  };

  /** The instances of this mod list that cannot open the world yet. */
  const shareCandidates = () => {
    const already = new Set(ways().map(link => link.instanceName));
    return props.modlistInstances.filter(name => !already.has(name));
  };

  return (
    <div class="flex items-center gap-4 p-4 rounded-xl bg-bgPanel border border-borderColor hover:border-primary/60 transition-colors">
      <WorldTile world={props.world} modlist={props.modlist} />

      <div class="min-w-0 flex-1">
        <div class="flex items-center gap-2 min-w-0">
          <span class="text-textMain font-semibold truncate" title={props.world.levelName}>
            {props.world.levelName}
          </span>
          {/* D96: the card says the world is shared, and with whom. */}
          <Show when={isShared()}>
            <span
              class="shrink-0 px-1.5 py-0.5 rounded text-[10px] font-semibold uppercase tracking-wide bg-primary/20 text-primary border border-primary/40 flex items-center gap-1"
              title={
                ways().length > 0
                  ? `Shared with ${ways().map(link => link.instanceName).join(", ")}`
                  : "Shared, but no instance can open it right now"
              }
            >
              <MaterialIcon name="link" size="sm" /> Shared
            </span>
          </Show>
          {/* Only when it is needed to tell two cards apart. */}
          <Show when={props.showFolderName}>
            <span class="shrink-0 text-xs text-textMuted truncate" title="The folder this world lives in">
              {props.world.folderName}
            </span>
          </Show>
        </div>

        <div class="flex items-center gap-1.5 text-sm text-textMain/90 truncate">
          <MaterialIcon name="folder_managed" size="sm" class="text-textMuted" />
          <span class="truncate">
            {props.modlist?.displayName || props.world.modlistName}
            <Show
              when={isShared()}
              fallback={<span class="text-textMuted"> · {props.world.instanceName}</span>}
            >
              <Show
                when={ways().length > 0}
                fallback={<span class="text-warning"> · no instance can open it</span>}
              >
                <span class="text-textMuted"> · {ways().map(link => link.instanceName).join(" · ")}</span>
              </Show>
            </Show>
          </span>
        </div>
        <div class="text-xs text-textMuted truncate">
          Singleplayer · {GAME_MODE_LABELS[props.world.gameMode]} · played {playedAgo(props.world.lastPlayedMs)}
        </div>
      </div>

      <div class="relative shrink-0" data-world-menu>
        <button
          class="px-4 h-9 rounded-lg bg-primary hover:bg-brandPurpleHover disabled:bg-muted disabled:text-textMuted disabled:cursor-not-allowed text-white text-sm font-medium flex items-center gap-1.5"
          disabled={ways().length === 0}
          title={
            ways().length === 0
              ? "No instance can open this world. Share it with one from the ⋮ menu."
              : undefined
          }
          onClick={() => start("world")}
        >
          <MaterialIcon name="play_arrow" size="sm" />
          Play
          <Show when={ways().length > 1}>
            <MaterialIcon name="expand_more" size="sm" />
          </Show>
        </button>

        <Show when={asking()}>
          <div class="absolute right-0 top-full mt-1 w-64 rounded-lg border border-borderColor bg-popover shadow-xl py-1 z-30">
            <div class="px-3 py-1.5 text-xs text-textMuted">
              {asking() === "world" ? "Open this world with" : "Play which instance"}
            </div>
            <For each={ways()}>
              {link => (
                <button
                  class="w-full px-3 py-2 text-left text-sm text-textMain hover:bg-bgHover flex items-center gap-2"
                  onClick={() => answer(link)}
                >
                  <MaterialIcon name="deployed_code" size="sm" /> {link.instanceName}
                </button>
              )}
            </For>
          </div>
        </Show>
      </div>

      <div class="relative shrink-0" data-world-menu>
        <button
          class="w-9 h-9 rounded-lg border border-borderColor text-textMuted hover:text-textMain hover:bg-bgHover flex items-center justify-center"
          aria-label={`More actions for ${props.world.levelName}`}
          onClick={props.onToggleMenu}
        >
          <MaterialIcon name="more_vert" size="sm" />
        </button>

        <Show when={props.menuOpen}>
          <div class="absolute right-0 top-full mt-1 w-72 max-h-96 overflow-y-auto rounded-lg border border-borderColor bg-popover shadow-xl py-1 z-30">
            <button
              class="w-full px-3 py-2 text-left text-sm text-textMain hover:bg-bgHover flex items-center gap-2"
              onClick={() => run(props.onOpenFolder)}
            >
              <MaterialIcon name="folder_open" size="sm" /> Open folder
            </button>
            <Show when={ways().length > 0}>
              <button
                class="w-full px-3 py-2 text-left text-sm text-textMain hover:bg-bgHover flex items-center gap-2"
                onClick={() => {
                  props.onCloseMenu();
                  start("modlist");
                }}
              >
                <MaterialIcon name="play_circle" size="sm" /> Play the mod list
              </button>
            </Show>
            <button
              class="w-full px-3 py-2 text-left text-sm text-textMain hover:bg-bgHover flex items-center gap-2"
              onClick={() => run(props.onOpenModlist)}
            >
              <MaterialIcon name="tune" size="sm" /> Go to the mod list
            </button>

            {/* ── Sharing (E4) ───────────────────────────────────────── */}
            <Show when={shareCandidates().length > 0}>
              <div class="my-1 border-t border-borderColor" />
              <div class="px-3 py-1.5 text-xs text-textMuted">
                {isShared() ? "Share with" : "Share with — the world moves to the mod list"}
              </div>
              <For each={shareCandidates()}>
                {instanceName => (
                  <button
                    class="w-full px-3 py-2 text-left text-sm text-textMain hover:bg-bgHover flex items-center gap-2"
                    onClick={() => run(() => props.onShareWith(instanceName))}
                  >
                    <MaterialIcon name="add_link" size="sm" /> {instanceName}
                  </button>
                )}
              </For>
            </Show>
            <Show when={isShared() && ways().length > 0}>
              <div class="my-1 border-t border-borderColor" />
              <div class="px-3 py-1.5 text-xs text-textMuted">Stop sharing with</div>
              <For each={ways()}>
                {link => (
                  <button
                    class="w-full px-3 py-2 text-left text-sm text-textMain hover:bg-bgHover flex items-center gap-2"
                    onClick={() => run(() => props.onUnshareFrom(link))}
                    title="The world stays in the mod list; only this instance's way in goes"
                  >
                    <MaterialIcon name="link_off" size="sm" /> {link.instanceName}
                  </button>
                )}
              </For>
            </Show>

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
  const [worlds, { refetch, mutate: mutateWorlds }] = createResource(async () => {
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

  /**
   * The id of the world whose ⋮ menu is open, or `null`.
   *
   * One signal for every card, because two dropdowns open at once is what a
   * per-card one produced: three of his worlds have the same name, so the
   * second menu opening while the first stayed up was genuinely confusing.
   */
  const [openMenuKey, setOpenMenuKey] = createSignal<string | null>(null);

  const closeOnOutside = (event: MouseEvent) => {
    if (!(event.target as HTMLElement | null)?.closest("[data-world-menu]")) setOpenMenuKey(null);
  };
  const closeOnEscape = (event: KeyboardEvent) => {
    if (event.key === "Escape") setOpenMenuKey(null);
  };
  document.addEventListener("click", closeOnOutside);
  document.addEventListener("keydown", closeOnEscape);
  onCleanup(() => {
    document.removeEventListener("click", closeOnOutside);
    document.removeEventListener("keydown", closeOnEscape);
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

  /**
   * Every instance directory of every mod list a world belongs to, so the ⋮
   * menu can offer the ones that cannot open the world yet.
   *
   * It reads the same instance roots the mod list's file browser reads. An
   * instance with no `saves/` is a perfectly good destination — it is the one
   * `26.3-neoforge` is — so the worlds themselves cannot be the source.
   */
  const [instancesByModlist] = createResource(
    () => [...new Set((worlds() ?? []).map(world => world.modlistName))].sort(),
    async (modlistNames) => {
      const byModlist: Record<string, string[]> = {};
      for (const modlistName of modlistNames) {
        try {
          const nodes = await invoke<{ name: string; isDir: boolean }[]>(
            "list_instance_files_command",
            { modlistName, relativePath: null },
          );
          byModlist[modlistName] = nodes.filter(node => node.isDir).map(node => node.name).sort();
        } catch {
          // A mod list whose instances cannot be read offers nothing to share
          // with, which is a smaller failure than a banner on the home.
          byModlist[modlistName] = [];
        }
      }
      return byModlist;
    },
  );

  /**
   * The level names carried by more than one visible world.
   *
   * Those cards get the folder name as well. Every card is not given one:
   * three of the four real worlds are called `New World`, but the fourth is
   * not, and a folder name on it would be noise.
   */
  const ambiguousNames = createMemo(() => {
    const seen = new Set<string>();
    const twice = new Set<string>();
    for (const world of visibleWorlds()) {
      if (seen.has(world.levelName)) twice.add(world.levelName);
      seen.add(world.levelName);
    }
    return twice;
  });

  /**
   * Both sharing commands answer with the listing as it now stands, so the
   * view is redrawn from what the disk says and not from what it believed.
   */
  const applyListing = (listing: WorldEntry[]) => {
    mutateWorlds(listing);
  };

  const shareWith = async (world: WorldEntry, instanceName: string) => {
    try {
      applyListing(
        await invoke<WorldEntry[]>("share_world_with_instance_command", {
          modlistName: world.modlistName,
          instanceName: world.instanceName,
          folderName: world.folderName,
          targetInstanceName: instanceName,
        }),
      );
    } catch (error) {
      pushUiError({
        title: "The world could not be shared",
        message: `'${world.levelName}' was not shared with ${instanceName}.`,
        detail: String(error),
        severity: "error",
        scope: "launch",
      });
      void refetch();
    }
  };

  const unshareFrom = async (world: WorldEntry, through: WorldInstanceLink) => {
    try {
      applyListing(
        await invoke<WorldEntry[]>("unshare_world_from_instance_command", {
          modlistName: world.modlistName,
          folderName: through.folderName,
          instanceName: through.instanceName,
        }),
      );
    } catch (error) {
      pushUiError({
        title: "The world is still shared",
        message: `${through.instanceName} still reaches '${world.levelName}'.`,
        detail: String(error),
        severity: "error",
        scope: "launch",
      });
      void refetch();
    }
  };

  return (
    <div class="flex-1 min-h-0 overflow-y-auto bg-bgDark">
      <div class="max-w-5xl mx-auto px-8 py-8 flex flex-col gap-10">
        {/* ── Jump in ─────────────────────────────────────────────────── */}
        <section>
          <h2 class="text-lg font-semibold text-textMain mb-1">Jump in</h2>
          <p class="text-sm text-textMuted mb-4">
            Your most recent worlds. Play opens the world itself on Minecraft 1.20 and newer;
            an older instance cannot skip the menu, and lands there instead.
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
                    showFolderName={ambiguousNames().has(world.levelName)}
                    modlistInstances={instancesByModlist()?.[world.modlistName] ?? []}
                    onPlay={through => props.onPlayWorld(world, through)}
                    onPlayModlist={through => props.onPlayModlistOf(world, through)}
                    onOpenModlist={() => props.onOpenModlist(world.modlistName)}
                    onOpenFolder={() => void openWorldFolder(world)}
                    onHide={() => void hideWorld(world)}
                    onShareWith={instanceName => void shareWith(world, instanceName)}
                    onUnshareFrom={through => void unshareFrom(world, through)}
                    menuOpen={openMenuKey() === worldKey(world)}
                    onToggleMenu={() =>
                      setOpenMenuKey(current => (current === worldKey(world) ? null : worldKey(world)))
                    }
                    onCloseMenu={() => setOpenMenuKey(null)}
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
