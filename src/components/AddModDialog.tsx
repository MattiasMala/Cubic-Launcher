import { Show, createSignal, createEffect } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import {
  addModModalOpen, setAddModModalOpen,
  addModSearch, setAddModSearch,
  addModMode, setAddModMode,
  modRowsState,
  activeContentTab, selectedModListName,
} from "../store";
import { SearchIcon, XIcon, UploadIcon, MaterialIcon } from "./icons";
import { LocalJarTab } from "./add-mod-dialog/LocalJarTab";
import { SearchSidebar } from "./add-mod-dialog/SearchSidebar";
import { SearchResults } from "./add-mod-dialog/SearchResults";
import type { AddModDialogProps, SearchFilters } from "./add-mod-dialog/shared";
import { searchModrinth } from "./add-mod-dialog/shared";

export function AddModDialog(props: AddModDialogProps) {
  const [searchResults, setSearchResults] = createSignal<any[]>([]);
  const [totalHits, setTotalHits] = createSignal(0);
  const [page, setPage] = createSignal(0);
  const [searching, setSearching] = createSignal(false);
  const [addingIds, setAddingIds] = createSignal<Set<string>>(new Set());
  const [addedIds, setAddedIds] = createSignal<Set<string>>(new Set());
  const [selCategories, setSelCategories] = createSignal<Set<string>>(new Set());
  const [selLoaders, setSelLoaders] = createSignal<Set<string>>(new Set());
  const [selVersions, setSelVersions] = createSignal<Set<string>>(new Set());
  const [selEnvs, setSelEnvs] = createSignal<Set<string>>(new Set());
  const [sortBy, setSortBy] = createSignal("relevance");
  const [contentType, setContentType] = createSignal<"mod" | "resourcepack" | "datapack" | "shader">("mod");
  const [showMoreCats, setShowMoreCats] = createSignal(false);
  const [existingContentIds, setExistingContentIds] = createSignal<Set<string>>(new Set());

  createEffect(() => {
    if (addModModalOpen()) {
      const tab = activeContentTab();
      setContentType(tab === "mods" ? "mod" : tab);
    }
  });

  createEffect(() => {
    const ct = contentType();
    const open = addModModalOpen();
    if (!open || ct === "mod") {
      setExistingContentIds(new Set<string>());
      return;
    }
    const modlist = selectedModListName();
    if (!modlist) return;
    void (async () => {
      try {
        const snap: any = await invoke("load_content_list_command", { input: { modlistName: modlist, contentType: ct } });
        const ids = new Set<string>((snap.entries ?? []).map((entry: any) => entry.id));
        setExistingContentIds(ids);
      } catch {
        setExistingContentIds(new Set<string>());
      }
    })();
  });

  const pageSize = 20;
  const totalPages = () => Math.max(1, Math.ceil(totalHits() / pageSize));

  const toggleSet = (setter: (fn: (s: Set<string>) => Set<string>) => void, value: string) => {
    setter(current => {
      const next = new Set(current);
      if (next.has(value)) next.delete(value);
      else next.add(value);
      return next;
    });
  };

  const filters = (): SearchFilters => ({
    categories: [...selCategories()],
    loaders: [...selLoaders()],
    versions: [...selVersions()],
    environments: [...selEnvs()],
    sortBy: sortBy(),
  });

  const projectType = () => contentType() === "datapack" ? "datapack" : contentType() === "resourcepack" ? "resourcepack" : contentType() === "shader" ? "shader" : "mod";
  const hasFilters = () => selCategories().size > 0 || selLoaders().size > 0 || selVersions().size > 0 || selEnvs().size > 0;

  const runSearch = async (pg?: number) => {
    const nextPage = pg ?? page();
    setSearching(true);
    try {
      const { results, totalHits: total } = await searchModrinth(addModSearch(), filters(), nextPage * pageSize, projectType());
      setSearchResults(results);
      setTotalHits(total);
    } catch {
      // ignore
    }
    setSearching(false);
  };

  let debounceTimer: ReturnType<typeof setTimeout> | undefined;
  createEffect(() => {
    addModSearch();
    filters();
    contentType();
    clearTimeout(debounceTimer);
    if (addModMode() !== "modrinth") return;

    setPage(0);
    debounceTimer = setTimeout(() => void runSearch(0), 350);
  });

  createEffect(() => {
    if (addModModalOpen()) {
      setAddedIds(new Set<string>());
      setAddingIds(new Set<string>());
      setPage(0);
      void runSearch(0);
    }
  });

  const existingModIds = () => {
    const ids = new Set<string>();
    const collect = (rows: any[]) => {
      for (const row of rows) {
        if (row.primaryModId) ids.add(row.primaryModId);
        if (row.modrinth_id) ids.add(row.modrinth_id);
        if (row.alternatives?.length) collect(row.alternatives);
      }
    };
    collect(modRowsState());
    return ids;
  };

  const handleAdd = async (id: string, name: string) => {
    setAddingIds(current => new Set([...current, id]));
    if (contentType() === "mod") {
      await props.onAddModrinth(id, name);
    } else if (props.onAddContent) {
      await props.onAddContent(contentType(), id, name);
    }
    setAddingIds(current => {
      const next = new Set(current);
      next.delete(id);
      return next;
    });
    setAddedIds(current => new Set([...current, id]));
    if (contentType() !== "mod") {
      setExistingContentIds(current => new Set([...current, id]));
    }
  };

  const close = () => {
    setAddModModalOpen(false);
    setAddModSearch("");
    setAddedIds(new Set<string>());
  };

  return (
    <Show when={addModModalOpen()}>
      <div
        class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 px-4 py-6 backdrop-blur-sm"
        onClick={e => { if (e.target === e.currentTarget) close(); }}
      >
        <div class="flex max-h-[90vh] w-full max-w-4xl flex-col overflow-hidden rounded-lg border border-border bg-card shadow-xl">
          <div class="flex items-center justify-between border-b border-border px-6 py-3">
            <div>
              <h2 class="text-lg font-semibold text-foreground">Add Content</h2>
            </div>
            <button
              onClick={close}
              class="flex h-8 w-8 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
            >
              <XIcon class="h-4 w-4" />
            </button>
          </div>

          <div class="flex items-center border-b border-border">
            <div class="flex items-center gap-1 px-4">
              <button
                onClick={() => setContentType("mod")}
                class={`flex items-center gap-1 px-3 py-2.5 text-xs font-medium rounded-t transition-colors ${contentType() === "mod" ? "bg-primary/10 text-primary border-b-2 border-primary" : "text-muted-foreground hover:text-foreground"}`}
              >
                <MaterialIcon name="extension" size="sm" />
                Mods
              </button>
              <button
                onClick={() => setContentType("resourcepack")}
                class={`flex items-center gap-1 px-3 py-2.5 text-xs font-medium rounded-t transition-colors ${contentType() === "resourcepack" ? "bg-primary/10 text-primary border-b-2 border-primary" : "text-muted-foreground hover:text-foreground"}`}
              >
                <MaterialIcon name="palette" size="sm" />
                Resource Packs
              </button>
              <button
                onClick={() => setContentType("datapack")}
                class={`flex items-center gap-1 px-3 py-2.5 text-xs font-medium rounded-t transition-colors ${contentType() === "datapack" ? "bg-primary/10 text-primary border-b-2 border-primary" : "text-muted-foreground hover:text-foreground"}`}
              >
                <MaterialIcon name="database" size="sm" />
                Data Packs
              </button>
              <button
                onClick={() => setContentType("shader")}
                class={`flex items-center gap-1 px-3 py-2.5 text-xs font-medium rounded-t transition-colors ${contentType() === "shader" ? "bg-primary/10 text-primary border-b-2 border-primary" : "text-muted-foreground hover:text-foreground"}`}
              >
                <MaterialIcon name="auto_awesome" size="sm" />
                Shaders
              </button>
            </div>
            <div class="ml-auto flex items-center border-l border-border">
              <button
                onClick={() => { setAddModMode("modrinth"); }}
                class={`flex items-center gap-1.5 px-4 py-2.5 text-xs font-medium transition-colors ${addModMode() === "modrinth" ? "text-primary" : "text-muted-foreground hover:text-foreground"}`}
              >
                <SearchIcon class="h-3.5 w-3.5" />
                Search
              </button>
              <button
                onClick={() => setAddModMode("local")}
                class={`flex items-center gap-1.5 px-4 py-2.5 text-xs font-medium transition-colors ${addModMode() === "local" ? "text-primary" : "text-muted-foreground hover:text-foreground"}`}
              >
                <UploadIcon class="h-3.5 w-3.5" />
                Upload
              </button>
            </div>
          </div>

          <div class="flex-1 min-h-0 flex flex-col overflow-hidden">
            <Show
              when={addModMode() === "modrinth"}
              fallback={
                <div class="flex-1 overflow-y-auto p-6"><LocalJarTab contentType={contentType()} onUploadLocal={props.onUploadLocal} onDropLocal={props.onDropLocal} /></div>
              }
            >
              <div class="flex flex-1 min-h-0">
                <SearchSidebar
                  sortBy={sortBy()}
                  setSortBy={setSortBy}
                  selLoaders={selLoaders()}
                  selEnvs={selEnvs()}
                  selCategories={selCategories()}
                  toggleLoaders={value => toggleSet(setSelLoaders, value)}
                  toggleEnvs={value => toggleSet(setSelEnvs, value)}
                  toggleCategories={value => toggleSet(setSelCategories, value)}
                  showMoreCats={showMoreCats()}
                  setShowMoreCats={setShowMoreCats}
                  hasFilters={hasFilters()}
                  clearFilters={() => { setSelCategories(new Set<string>()); setSelLoaders(new Set<string>()); setSelVersions(new Set<string>()); setSelEnvs(new Set<string>()); }}
                />

                <SearchResults
                  contentType={contentType()}
                  searching={searching()}
                  searchResults={searchResults()}
                  totalPages={totalPages()}
                  page={page()}
                  setSearch={setAddModSearch}
                  previousPage={() => { const nextPage = Math.max(0, page() - 1); setPage(nextPage); void runSearch(nextPage); }}
                  nextPage={() => { const nextPage = Math.min(totalPages() - 1, page() + 1); setPage(nextPage); void runSearch(nextPage); }}
                  handleAdd={handleAdd}
                  addingIds={addingIds()}
                  addedIds={addedIds()}
                  existingIds={new Set([...existingModIds(), ...existingContentIds()])}
                />
              </div>
            </Show>
          </div>

          <div class="flex justify-end border-t border-border px-6 py-4">
            <button
              onClick={close}
              class="rounded-md bg-secondary px-4 py-2 text-sm font-medium text-secondary-foreground transition-colors hover:bg-secondary/80"
            >
              Done
            </button>
          </div>
        </div>
      </div>
    </Show>
  );
}
