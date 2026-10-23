import { For, Show } from "solid-js";
import {
  SEARCH_CATEGORIES,
  SEARCH_ENVIRONMENTS,
  SEARCH_LOADERS,
  SORT_OPTIONS,
  categoryLabel,
  loaderLabel,
  type SearchContentType,
} from "./shared";
import { Select } from "../Select";

export function SearchSidebar(props: {
  contentType: SearchContentType;
  sortBy: string;
  setSortBy: (value: string) => void;
  /**
   * D149: the search follows the mod list's Minecraft version (and, for mods,
   * its loader) unless the user turns it off. `compatLabel` says what it is
   * filtering on, e.g. «1.21.1 · Fabric».
   */
  compatFilter: boolean;
  setCompatFilter: (value: boolean) => void;
  compatLabel: string;
  /** True while the compatibility filter already decides the loader: the manual loader boxes are then off. */
  loaderFromModList: boolean;
  selLoaders: Set<string>;
  selEnvs: Set<string>;
  selCategories: Set<string>;
  toggleLoaders: (value: string) => void;
  toggleEnvs: (value: string) => void;
  toggleCategories: (value: string) => void;
  showMoreCats: boolean;
  setShowMoreCats: (value: boolean) => void;
  hasFilters: boolean;
  clearFilters: () => void;
}) {
  const loaders = () => SEARCH_LOADERS[props.contentType];
  const environments = () => SEARCH_ENVIRONMENTS[props.contentType];
  const categories = () => SEARCH_CATEGORIES[props.contentType];

  return (
    <div class="w-48 shrink-0 border-r border-border overflow-y-auto p-3 space-y-3">
      <div>
        <h4 class="text-[10px] font-semibold uppercase tracking-wider text-muted-foreground mb-1.5">Sort by</h4>
        <Select
          value={props.sortBy}
          options={SORT_OPTIONS.map(option => ({ value: option.value, label: option.label }))}
          onChange={value => props.setSortBy(value)}
          class="w-full rounded border border-border bg-input px-2 py-1 text-xs text-foreground"
        />
      </div>

      <div>
        <h4 class="text-[10px] font-semibold uppercase tracking-wider text-muted-foreground mb-1.5">Compatibility</h4>
        <label class="flex items-start gap-1.5 cursor-pointer text-xs text-foreground hover:text-primary transition-colors">
          <input
            type="checkbox"
            checked={props.compatFilter}
            onChange={event => props.setCompatFilter(event.currentTarget.checked)}
            class="accent-accentColor w-3 h-3 mt-0.5"
          />
          <span>
            Only for this mod list
            <span class="block text-[10px] text-muted-foreground">{props.compatLabel}</span>
          </span>
        </label>
      </div>

      <Show when={loaders().length > 0}>
        <div>
          <h4 class="text-[10px] font-semibold uppercase tracking-wider text-muted-foreground mb-1.5">Loaders</h4>
          <Show
            when={!props.loaderFromModList}
            fallback={<p class="text-[10px] text-muted-foreground">Set by the mod list. Turn off the compatibility filter to pick another loader.</p>}
          >
            <div class="space-y-1">
              <For each={loaders()}>
                {loader => (
                  <label class="flex items-center gap-1.5 cursor-pointer text-xs text-foreground hover:text-primary transition-colors">
                    <input type="checkbox" checked={props.selLoaders.has(loader)} onChange={() => props.toggleLoaders(loader)} class="accent-accentColor w-3 h-3" />
                    {loaderLabel(loader)}
                  </label>
                )}
              </For>
            </div>
          </Show>
        </div>
      </Show>

      <Show when={environments().length > 0}>
        <div>
          <h4 class="text-[10px] font-semibold uppercase tracking-wider text-muted-foreground mb-1.5">Environment</h4>
          <div class="space-y-1">
            <For each={environments()}>
              {env => (
                <label class="flex items-center gap-1.5 cursor-pointer text-xs text-foreground hover:text-primary transition-colors">
                  <input type="checkbox" checked={props.selEnvs.has(env)} onChange={() => props.toggleEnvs(env)} class="accent-accentColor w-3 h-3" />
                  {env.charAt(0).toUpperCase() + env.slice(1)}
                </label>
              )}
            </For>
          </div>
        </div>
      </Show>

      <div>
        <h4 class="text-[10px] font-semibold uppercase tracking-wider text-muted-foreground mb-1.5">Categories</h4>
        <div class="space-y-1">
          <For each={props.showMoreCats ? categories() : categories().slice(0, 8)}>
            {category => (
              <label class="flex items-center gap-1.5 cursor-pointer text-xs text-foreground hover:text-primary transition-colors">
                <input type="checkbox" checked={props.selCategories.has(category)} onChange={() => props.toggleCategories(category)} class="accent-accentColor w-3 h-3" />
                {categoryLabel(category)}
              </label>
            )}
          </For>
          <Show when={categories().length > 8}>
            <button onClick={() => props.setShowMoreCats(!props.showMoreCats)} class="text-[10px] text-primary hover:underline">
              {props.showMoreCats ? "Show less" : `Show all (${categories().length})`}
            </button>
          </Show>
        </div>
      </div>

      {props.hasFilters && (
        <button
          onClick={props.clearFilters}
          class="w-full rounded border border-dashed border-border px-2 py-1 text-[10px] text-muted-foreground hover:text-foreground hover:border-primary/40 transition-colors"
        >
          Clear all filters
        </button>
      )}
    </div>
  );
}
