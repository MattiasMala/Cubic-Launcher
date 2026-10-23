import { For, Show } from "solid-js";
import { addModSearch } from "../../store";
import type { ModrinthResult } from "../../lib/types";
import { SearchIcon, XIcon, PackageIcon, CheckIcon, Loader2Icon } from "../icons";
import { formatDownloads, type SearchContentType } from "./shared";

/** What a tab's results are called, for the empty and error states (todo.md:1720). */
const RESULT_NOUN: Record<SearchContentType, string> = {
  mod: "mods",
  resourcepack: "resource packs",
  datapack: "data packs",
  shader: "shaders",
};

function PaginationControls(props: {
  totalPages: number;
  page: number;
  previousPage: () => void;
  nextPage: () => void;
}) {
  return (
    <Show when={props.totalPages > 1}>
      <div class="flex items-center justify-center gap-3 pt-3 pb-1">
        <button
          onClick={props.previousPage}
          disabled={props.page === 0}
          class="rounded-md border border-border px-3 py-1 text-xs text-foreground hover:bg-muted disabled:opacity-30 disabled:cursor-not-allowed transition-colors"
        >
          Previous
        </button>
        <span class="text-xs text-muted-foreground">
          Page {props.page + 1} of {props.totalPages}
        </span>
        <button
          onClick={props.nextPage}
          disabled={props.page >= props.totalPages - 1}
          class="rounded-md border border-border px-3 py-1 text-xs text-foreground hover:bg-muted disabled:opacity-30 disabled:cursor-not-allowed transition-colors"
        >
          Next
        </button>
      </div>
    </Show>
  );
}

export function SearchResults(props: {
  contentType: SearchContentType;
  searching: boolean;
  /** Why the last search failed; the results are then empty, never made up. */
  searchError: string | null;
  retrySearch: () => void;
  /** Set when the entries already in the mod list could not be read: «Added» is then incomplete. */
  existingError: string | null;
  searchResults: ModrinthResult[];
  totalPages: number;
  page: number;
  setSearch: (value: string) => void;
  previousPage: () => void;
  nextPage: () => void;
  handleAdd: (id: string, name: string) => Promise<void>;
  addingIds: Set<string>;
  addedIds: Set<string>;
  /** Results whose add failed, with the reason; clicking one tries again. */
  failedIds: Map<string, string>;
  existingIds: Set<string>;
}) {
  const noun = () => RESULT_NOUN[props.contentType];
  return (
    <div class="flex-1 min-h-0 overflow-y-auto p-4">
      <div class="relative mb-3">
        <SearchIcon class="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground" />
        <Show when={addModSearch()}>
          <button
            onClick={() => props.setSearch("")}
            class="absolute right-3 top-1/2 -translate-y-1/2 text-muted-foreground hover:text-foreground transition-colors"
          >
            <XIcon class="h-4 w-4" />
          </button>
        </Show>
        <input
          type="text"
          placeholder={props.contentType === "mod" ? "Search mods..." : props.contentType === "resourcepack" ? "Search resource packs..." : props.contentType === "datapack" ? "Search data packs..." : "Search shaders..."}
          value={addModSearch()}
          onInput={e => props.setSearch(e.currentTarget.value)}
          class="h-10 w-full rounded-xl border border-input bg-input pl-10 pr-10 text-sm text-foreground placeholder:text-muted-foreground focus:outline-none focus:ring-2 focus:ring-primary/50 focus:border-primary transition-all"
          autofocus
        />
      </div>

      <Show when={props.existingError}>
        <div class="mb-3 rounded-md border border-warning/40 bg-warning/10 px-3 py-2 text-xs text-muted-foreground" role="status">
          The {noun()} already in this mod list could not be read, so they are not marked «Added». {props.existingError}
        </div>
      </Show>

      <div class="space-y-1.5">
        <PaginationControls
          totalPages={props.totalPages}
          page={props.page}
          previousPage={props.previousPage}
          nextPage={props.nextPage}
        />
        <For each={props.searchResults}>
          {mod => {
            const isAdded = () => props.addedIds.has(mod.id) || props.existingIds.has(mod.id);
            const isAdding = () => props.addingIds.has(mod.id);
            const failure = () => (isAdded() || isAdding()) ? undefined : props.failedIds.get(mod.id);

            return (
              <div class="flex items-center gap-3 rounded-lg p-2.5 transition-colors hover:bg-muted/40 cursor-pointer group" onClick={() => { if (!isAdded() && !isAdding()) void props.handleAdd(mod.id, mod.name); }}>
                <Show when={mod.iconUrl} fallback={
                  <div class="flex h-11 w-11 shrink-0 items-center justify-center rounded-lg bg-muted">
                    <PackageIcon class="h-5 w-5 text-muted-foreground" />
                  </div>
                }>
                  <img src={mod.iconUrl} alt="" class="h-11 w-11 shrink-0 rounded-lg object-cover bg-muted" loading="lazy" />
                </Show>
                <div class="min-w-0 flex-1">
                  <div class="flex items-center gap-2">
                    <span class="font-semibold text-sm text-foreground truncate">{mod.name}</span>
                    <span class="text-xs text-muted-foreground shrink-0">by {mod.author}</span>
                  </div>
                  <p class="line-clamp-1 text-xs text-muted-foreground mt-0.5">{mod.description}</p>
                  <div class="flex items-center gap-2 mt-0.5">
                    <Show when={mod.downloads != null}>
                      <span class="text-[10px] text-muted-foreground/70">{formatDownloads(mod.downloads!)} downloads</span>
                    </Show>
                    <For each={mod.categories.slice(0, 3)}>
                      {cat => (
                        <span class="rounded px-1 py-px text-[10px] text-muted-foreground/70 bg-muted">
                          {cat}
                        </span>
                      )}
                    </For>
                  </div>
                </div>
                <div class="shrink-0">
                  <Show when={isAdded()}>
                    <span class="flex h-8 items-center gap-1 rounded-lg px-3 text-xs font-medium bg-green-500/15 text-green-500">
                      <CheckIcon class="h-3.5 w-3.5" />
                      Added
                    </span>
                  </Show>
                  <Show when={isAdding()}>
                    <span class="flex h-8 items-center rounded-lg px-3">
                      <Loader2Icon class="h-4 w-4 animate-spin text-muted-foreground" />
                    </span>
                  </Show>
                  <Show when={failure()}>
                    <span
                      class="flex h-8 items-center gap-1 rounded-lg px-3 text-xs font-medium bg-destructive/15 text-destructive"
                      title={failure()}
                    >
                      <XIcon class="h-3.5 w-3.5" />
                      Failed - retry
                    </span>
                  </Show>
                  <Show when={!isAdded() && !isAdding() && !failure()}>
                    <button
                      class="flex h-8 items-center rounded-lg px-3 text-xs font-medium bg-primary text-primary-foreground hover:bg-primary/90 opacity-0 group-hover:opacity-100 transition-opacity"
                    >
                      Add
                    </button>
                  </Show>
                </div>
              </div>
            );
          }}
        </For>
        <Show when={props.searchError && !props.searching}>
          <div class="flex flex-col items-center justify-center py-12 text-center" role="alert">
            <PackageIcon class="mb-4 h-12 w-12 text-destructive/50" />
            <p class="text-foreground">Could not search Modrinth</p>
            <p class="mt-1 max-w-md text-sm text-muted-foreground/80">{props.searchError}</p>
            <button
              onClick={props.retrySearch}
              class="mt-4 rounded-md bg-secondary px-4 py-1.5 text-sm font-medium text-secondary-foreground transition-colors hover:bg-secondary/80"
            >
              Try again
            </button>
          </div>
        </Show>
        <Show when={props.searchResults.length === 0 && !props.searching && !props.searchError}>
          <div class="flex flex-col items-center justify-center py-12 text-center">
            <PackageIcon class="mb-4 h-12 w-12 text-muted-foreground/40" />
            <p class="text-muted-foreground">
              <Show when={addModSearch().trim()} fallback={<>No {noun()} found</>}>
                No {noun()} found for "{addModSearch()}"
              </Show>
            </p>
            <p class="mt-1 text-sm text-muted-foreground/60">
              Try a different search term or fewer filters
            </p>
          </div>
        </Show>

        <PaginationControls
          totalPages={props.totalPages}
          page={props.page}
          previousPage={props.previousPage}
          nextPage={props.nextPage}
        />
      </div>
    </div>
  );
}
