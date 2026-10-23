import { Show, createSignal, createEffect, on, onCleanup, untrack } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import {
  addModModalOpen, setAddModModalOpen,
  addModSearch, setAddModSearch,
  addModMode, setAddModMode,
  modRowsState,
  activeContentTab, selectedModListName,
  activePlaysOffline,
  selectedMcVersion, selectedModLoader,
} from "../store";
import { isTauri } from "../app/backend-loaders";
import { SearchIcon, XIcon, UploadIcon, MaterialIcon } from "./icons";
import { LocalJarTab } from "./add-mod-dialog/LocalJarTab";
import { SearchSidebar } from "./add-mod-dialog/SearchSidebar";
import { SearchResults } from "./add-mod-dialog/SearchResults";
import type { AddModDialogProps, SearchContentType, SearchFilters } from "./add-mod-dialog/shared";
import { compatibleSearchLoaders, searchModrinth } from "./add-mod-dialog/shared";

export function AddModDialog(props: AddModDialogProps) {
  const [searchResults, setSearchResults] = createSignal<any[]>([]);
  const [totalHits, setTotalHits] = createSignal(0);
  const [page, setPage] = createSignal(0);
  const [searching, setSearching] = createSignal(false);
  const [searchError, setSearchError] = createSignal<string | null>(null);
  const [addingIds, setAddingIds] = createSignal<Set<string>>(new Set());
  const [addedIds, setAddedIds] = createSignal<Set<string>>(new Set());
  const [failedIds, setFailedIds] = createSignal<Map<string, string>>(new Map());
  const [selCategories, setSelCategories] = createSignal<Set<string>>(new Set());
  const [selLoaders, setSelLoaders] = createSignal<Set<string>>(new Set());
  const [selEnvs, setSelEnvs] = createSignal<Set<string>>(new Set());
  // D149: by default the search shows only what fits the mod list's
  // Minecraft version (and loader, for mods); the sidebar can turn it off.
  const [compatFilter, setCompatFilter] = createSignal(true);
  const [sortBy, setSortBy] = createSignal("relevance");
  const [contentType, setContentType] = createSignal<SearchContentType>("mod");
  const [showMoreCats, setShowMoreCats] = createSignal(false);
  const [existingContentIds, setExistingContentIds] = createSignal<Set<string>>(new Set());
  const [existingError, setExistingError] = createSignal<string | null>(null);

  createEffect(() => {
    if (addModModalOpen()) {
      const tab = activeContentTab();
      setContentType(tab === "mods" ? "mod" : tab);
    }
  });

  // Loader, categorie e ambiente sono di un tipo di contenuto: passando a
  // un'altra tab si azzerano, o un «Fabric» spuntato nei Mods svuoterebbe la
  // ricerca degli shader (audit 101, rete-6).
  createEffect(on(contentType, () => {
    setSelCategories(new Set<string>());
    setSelLoaders(new Set<string>());
    setSelEnvs(new Set<string>());
    setShowMoreCats(false);
    setFailedIds(new Map<string, string>());
  }, { defer: true }));

  // Le voci già nella mod list, per il badge «Added». Una lettura fallita si
  // dice invece di sembrare una lista vuota, e una lettura per una tab già
  // lasciata non scrive sopra quella nuova.
  let existingSeq = 0;
  createEffect(() => {
    const ct = contentType();
    const open = addModModalOpen();
    const seq = ++existingSeq;
    setExistingError(null);
    if (!open || ct === "mod") {
      setExistingContentIds(new Set<string>());
      return;
    }
    const modlist = selectedModListName();
    if (!modlist || !isTauri()) return;
    void (async () => {
      try {
        const snap: any = await invoke("load_content_list_command", { input: { modlistName: modlist, contentType: ct } });
        if (seq !== existingSeq) return;
        const ids = new Set<string>((snap.entries ?? []).map((entry: any) => entry.id));
        setExistingContentIds(ids);
      } catch (error) {
        if (seq !== existingSeq) return;
        setExistingContentIds(new Set<string>());
        setExistingError(String(error));
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

  const compatLoaders = () => contentType() === "mod" ? compatibleSearchLoaders(selectedModLoader(), selectedMcVersion()) : [];
  const loaderFromModList = () => compatFilter() && compatLoaders().length > 0;
  const compatLabel = () => {
    const version = selectedMcVersion() || "any version";
    return compatLoaders().length > 0 ? `${version} · ${selectedModLoader()}` : `Minecraft ${version}`;
  };

  const filters = (): SearchFilters => ({
    categories: [...selCategories()],
    loaders: loaderFromModList() ? compatLoaders() : [...selLoaders()],
    versions: compatFilter() && selectedMcVersion() ? [selectedMcVersion()] : [],
    environments: [...selEnvs()],
    sortBy: sortBy(),
  });

  const hasFilters = () => selCategories().size > 0 || selLoaders().size > 0 || selEnvs().size > 0;

  // Solo l'ultima ricerca scrive i risultati: quella prima viene interrotta,
  // e se risponde lo stesso la sua risposta si butta (audit 101, rete-7).
  let searchSeq = 0;
  let searchAbort: AbortController | undefined;

  const runSearch = async (pg?: number) => {
    // D140: the search is Modrinth's; an account that plays offline asks nobody.
    if (activePlaysOffline()) return;
    const nextPage = pg ?? page();
    const seq = ++searchSeq;
    searchAbort?.abort();
    const controller = new AbortController();
    searchAbort = controller;
    setSearching(true);
    try {
      const { results, totalHits: total } = await searchModrinth(addModSearch(), filters(), nextPage * pageSize, contentType(), controller.signal);
      if (seq !== searchSeq) return;
      setSearchResults(results);
      setTotalHits(total);
      setSearchError(null);
    } catch (error) {
      if (seq !== searchSeq) return;
      // Nessun risultato inventato: la lista resta vuota e si dice perché
      // (audit 101, todo.md:1486).
      setSearchResults([]);
      setTotalHits(0);
      const reason = error instanceof Error ? error.message : String(error);
      setSearchError(`Modrinth could not be reached or refused the search. ${reason}`);
    } finally {
      if (seq === searchSeq) setSearching(false);
    }
  };

  let debounceTimer: ReturnType<typeof setTimeout> | undefined;
  createEffect(() => {
    addModSearch();
    filters();
    contentType();
    clearTimeout(debounceTimer);
    // The dialog is always mounted, and the filters follow the mod list's
    // version and loader (D149): closed, a change there is no search
    // (review-frontend-shell-22). Opening runs the first one, so the open
    // state is read untracked.
    if (!untrack(addModModalOpen) || addModMode() !== "modrinth") return;

    setPage(0);
    debounceTimer = setTimeout(() => void runSearch(0), 350);
  });

  // Only the opening resets the dialog: `runSearch` reads the query and the
  // filters, and without `on` this effect would track them and wipe the
  // «Added» badges on every keystroke.
  createEffect(on(addModModalOpen, open => {
    if (open) {
      setAddedIds(new Set<string>());
      setAddingIds(new Set<string>());
      setFailedIds(new Map<string, string>());
      setCompatFilter(true);
      setPage(0);
      // Offline only the upload works: open on it.
      if (activePlaysOffline()) setAddModMode("local");
      void runSearch(0);
    } else {
      clearTimeout(debounceTimer);
      searchSeq += 1;
      searchAbort?.abort();
      setSearching(false);
    }
  }));

  onCleanup(() => {
    clearTimeout(debounceTimer);
    searchAbort?.abort();
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

  // «Added» solo quando l'aggiunta è sul disco: un rifiuto lascia la riga
  // cliccabile con il motivo (audit 101, todo.md:1711).
  const handleAdd = async (id: string, name: string) => {
    const type = contentType();
    setAddingIds(current => new Set([...current, id]));
    setFailedIds(current => {
      const next = new Map(current);
      next.delete(id);
      return next;
    });
    let failure: string | null;
    try {
      if (type === "mod") failure = await props.onAddModrinth(id, name);
      else if (props.onAddContent) failure = await props.onAddContent(type, id, name);
      else failure = "Packs cannot be added from here.";
    } catch (error) {
      failure = String(error);
    }
    setAddingIds(current => {
      const next = new Set(current);
      next.delete(id);
      return next;
    });
    if (failure !== null) {
      setFailedIds(current => new Map(current).set(id, failure));
      return;
    }
    setAddedIds(current => new Set([...current, id]));
    if (type !== "mod" && contentType() === type) {
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
                disabled={activePlaysOffline()}
                title={activePlaysOffline() ? "This account plays offline, and the search asks Modrinth. Turn off Play offline in Manage Accounts to search; Upload works offline." : undefined}
                class={`flex items-center gap-1.5 px-4 py-2.5 text-xs font-medium transition-colors disabled:cursor-not-allowed disabled:opacity-50 ${addModMode() === "modrinth" ? "text-primary" : "text-muted-foreground hover:text-foreground"}`}
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
                  contentType={contentType()}
                  sortBy={sortBy()}
                  setSortBy={setSortBy}
                  compatFilter={compatFilter()}
                  setCompatFilter={setCompatFilter}
                  compatLabel={compatLabel()}
                  loaderFromModList={loaderFromModList()}
                  selLoaders={selLoaders()}
                  selEnvs={selEnvs()}
                  selCategories={selCategories()}
                  toggleLoaders={value => toggleSet(setSelLoaders, value)}
                  toggleEnvs={value => toggleSet(setSelEnvs, value)}
                  toggleCategories={value => toggleSet(setSelCategories, value)}
                  showMoreCats={showMoreCats()}
                  setShowMoreCats={setShowMoreCats}
                  hasFilters={hasFilters()}
                  clearFilters={() => { setSelCategories(new Set<string>()); setSelLoaders(new Set<string>()); setSelEnvs(new Set<string>()); }}
                />

                <SearchResults
                  contentType={contentType()}
                  searching={searching()}
                  searchError={searchError()}
                  retrySearch={() => void runSearch()}
                  existingError={existingError()}
                  searchResults={searchResults()}
                  totalPages={totalPages()}
                  page={page()}
                  setSearch={setAddModSearch}
                  previousPage={() => { const nextPage = Math.max(0, page() - 1); setPage(nextPage); void runSearch(nextPage); }}
                  nextPage={() => { const nextPage = Math.min(totalPages() - 1, page() + 1); setPage(nextPage); void runSearch(nextPage); }}
                  handleAdd={handleAdd}
                  addingIds={addingIds()}
                  addedIds={addedIds()}
                  failedIds={failedIds()}
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
