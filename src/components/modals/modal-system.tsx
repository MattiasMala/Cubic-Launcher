import { For, Show, createEffect, createSignal, on } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import {
  errorCenterOpen, setErrorCenterOpen, launcherErrors, dismissError,
  exportModalOpen, setExportModalOpen, exportOptions, setExportOptions,
  selectedModListName,
} from "../../store";
import { AlertTriangleIcon, ChevronRightIcon, ChevronDownIcon, MaterialIcon, XIcon } from "../icons";
import { Modal, ModalHeader } from "./modal-base";

type FileNode = { name: string; path: string; isDir: boolean; children: FileNode[] };

function FileTreeNode(props: { node: FileNode; modlistName: string; selected: Set<string>; onToggle: (path: string) => void; depth?: number }) {
  const [expanded, setExpanded] = createSignal(false);
  const [children, setChildren] = createSignal<FileNode[]>([]);
  const [loaded, setLoaded] = createSignal(false);
  const [error, setError] = createSignal("");
  const isSelected = () => props.selected.has(props.node.path);
  const depth = () => props.depth ?? 0;

  const toggleExpand = async (e: MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    if (!loaded() && props.node.isDir) {
      try {
        const nodes: FileNode[] = await invoke("list_instance_files_command", {
          modlistName: props.modlistName,
          relativePath: props.node.path,
        });
        setChildren(nodes);
        setError("");
        setLoaded(true);
      } catch (err) {
        // Said, not shown as "Empty": the folder may well have files.
        setChildren([]);
        setError(String(err));
      }
    }
    setExpanded(value => !value);
  };

  return (
    <div style={{ "padding-left": `${depth() * 16}px` }}>
      <div class="flex items-center gap-1.5 py-0.5 text-xs hover:bg-muted/30 rounded px-1">
        <Show when={props.node.isDir} fallback={<span class="w-3 shrink-0" />}>
          <button class="shrink-0 text-muted-foreground" onClick={toggleExpand}>
            <Show when={expanded()} fallback={<ChevronRightIcon class="h-3 w-3" />}><ChevronDownIcon class="h-3 w-3" /></Show>
          </button>
        </Show>
        <input
          type="checkbox"
          checked={isSelected()}
          onChange={() => props.onToggle(props.node.path)}
          class="h-3 w-3 rounded text-primary cursor-pointer"
        />
        <MaterialIcon name={props.node.isDir ? "folder" : "description"} size="sm" class={props.node.isDir ? "text-primary" : "text-muted-foreground"} />
        <span class="text-foreground truncate">{props.node.name}</span>
      </div>
      <Show when={expanded()}>
        <For each={children()}>
          {child => <FileTreeNode node={child} modlistName={props.modlistName} selected={props.selected} onToggle={props.onToggle} depth={depth() + 1} />}
        </For>
        <Show when={error()}>
          <div style={{ "padding-left": `${(depth() + 1) * 16}px` }} class="text-[10px] text-destructive py-0.5 px-1 break-all">Could not list this folder: {error()}</div>
        </Show>
        <Show when={loaded() && !error() && children().length === 0}>
          <div style={{ "padding-left": `${(depth() + 1) * 16}px` }} class="text-[10px] text-muted-foreground py-0.5 px-1">Empty</div>
        </Show>
      </Show>
    </div>
  );
}

type LocalPack = { name: string; path: string };

/**
 * One content category of the export. The checkbox carries the mod list's own
 * folder for the category (`resourcepacks/`, `datapacks/`, `shaders/`), never
 * the copies an instance sees through its links (`modlist-4`). The packs listed
 * under it are the local ones in that folder: picking some exports only those,
 * picking none exports them all (D150). Modrinth packs are not files of the mod
 * list: they are in its pack list, which always travels.
 */
function ContentSubSection(subProps: {
  label: string;
  contentType: string;
  categoryDir: string;
  modlistName: string;
  checked: boolean;
  onCheck: (value: boolean) => void;
  open: boolean;
  onToggleOpen: () => void;
  selectedPaths: () => Set<string>;
  onTogglePath: (path: string) => void;
}) {
  const [packs, setPacks] = createSignal<LocalPack[]>([]);
  const [loadedFor, setLoadedFor] = createSignal<string | null>(null);
  const [loadError, setLoadError] = createSignal("");

  createEffect(() => {
    const modlist = subProps.modlistName;
    if (!subProps.open || !modlist || loadedFor() === modlist) return;
    setLoadedFor(modlist);
    setPacks([]);
    setLoadError("");
    void (async () => {
      try {
        const snap = await invoke<{ entries: { source: string; name?: string | null; fileName?: string | null }[] }>(
          "load_content_list_command",
          { input: { modlistName: modlist, contentType: subProps.contentType } },
        );
        if (subProps.modlistName !== modlist) return;
        setPacks(
          snap.entries
            .filter(entry => entry.source === "local" && entry.fileName)
            .map(entry => ({ name: entry.name || entry.fileName!, path: `${subProps.categoryDir}/${entry.fileName}` })),
        );
      } catch (err) {
        if (subProps.modlistName === modlist) setLoadError(String(err));
      }
    })();
  });

  const pickedHere = () => packs().filter(pack => subProps.selectedPaths().has(pack.path)).length;

  return (
    <div>
      <div class="flex items-center gap-2">
        <label class="flex items-center gap-3 text-sm flex-1">
          <input type="checkbox" checked={subProps.checked} onChange={e => subProps.onCheck(e.currentTarget.checked)} class="h-3.5 w-3.5 rounded text-primary" />
          <span class="text-muted-foreground">{subProps.label}</span>
        </label>
        <button onClick={subProps.onToggleOpen} class="text-muted-foreground hover:text-foreground transition-colors p-0.5">
          <Show when={subProps.open} fallback={<ChevronRightIcon class="h-3 w-3" />}><ChevronDownIcon class="h-3 w-3" /></Show>
        </button>
      </div>
      <Show when={subProps.open}>
        <div class="ml-6 mt-1 space-y-0.5 max-h-32 overflow-y-auto">
          <Show when={loadError()}>
            <p class="text-[10px] text-destructive py-1 break-all">Could not read the {subProps.label.toLowerCase()} of this mod list: {loadError()}</p>
          </Show>
          <Show when={!loadError()}>
            <Show
              when={packs().length > 0}
              fallback={<p class="text-[10px] text-muted-foreground py-1">{loadedFor() === subProps.modlistName ? "No local packs. Modrinth packs travel in the list and download automatically." : "Loading..."}</p>}
            >
              <p class="text-[10px] text-muted-foreground py-0.5">
                {pickedHere() === 0 ? "None picked: every local pack below is exported." : `Only the ${pickedHere()} picked pack${pickedHere() !== 1 ? "s" : ""} will be exported.`}
              </p>
              <For each={packs()}>
                {pack => (
                  <label class="flex items-center gap-2 text-xs py-0.5 cursor-pointer hover:bg-muted/30 rounded px-1">
                    <input
                      type="checkbox"
                      checked={subProps.selectedPaths().has(pack.path)}
                      onChange={() => subProps.onTogglePath(pack.path)}
                      class="h-3 w-3 rounded text-primary"
                    />
                    <MaterialIcon name="description" size="sm" class="text-muted-foreground" />
                    <span class="text-muted-foreground truncate">{pack.name}</span>
                  </label>
                )}
              </For>
            </Show>
          </Show>
        </div>
      </Show>
    </div>
  );
}

export function ErrorCenter() {
  return (
    <Show when={errorCenterOpen()}>
      <Modal onClose={() => setErrorCenterOpen(false)}>
        <ModalHeader title="Error Center" description="All launcher warnings and errors" onClose={() => setErrorCenterOpen(false)} />
        <div class="flex-1 overflow-y-auto p-6 space-y-3">
          <Show when={launcherErrors().length === 0}>
            <p class="text-center text-sm text-muted-foreground py-8">No errors or warnings.</p>
          </Show>
          <For each={launcherErrors()}>
            {err => (
              <div class={`rounded-md border p-4 ${err.severity === "error" ? "border-destructive/40 bg-destructive/10" : "border-warning/40 bg-warning/10"}`}>
                <div class="flex items-start justify-between gap-3">
                  <div class="flex items-start gap-2">
                    <AlertTriangleIcon class={`mt-0.5 h-4 w-4 shrink-0 ${err.severity === "error" ? "text-destructive" : "text-warning"}`} />
                    <div>
                      <p class={`font-medium text-sm ${err.severity === "error" ? "text-destructive" : "text-warning"}`}>{err.title}</p>
                      <p class="mt-1 text-sm text-foreground">{err.message}</p>
                      <p class="mt-1 text-xs text-muted-foreground">{err.detail}</p>
                    </div>
                  </div>
                  <button onClick={() => dismissError(err.id)} class="flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-muted-foreground hover:bg-muted hover:text-foreground">
                    <XIcon class="h-3.5 w-3.5" />
                  </button>
                </div>
              </div>
            )}
          </For>
        </div>
        <div class="flex justify-end border-t border-border px-6 py-4">
          <button onClick={() => setErrorCenterOpen(false)} class="rounded-md bg-secondary px-4 py-2 text-sm text-secondary-foreground hover:bg-secondary/80">Close</button>
        </div>
      </Modal>
    </Show>
  );
}

export function ExportModal(props: { onExport: () => Promise<void> }) {
  const [saving, setSaving] = createSignal(false);
  const [contentOpen, setContentOpen] = createSignal(false);
  const [resourcePacksOpen, setResourcePacksOpen] = createSignal(false);
  const [dataPacksOpen, setDataPacksOpen] = createSignal(false);
  const [shadersOpen, setShadersOpen] = createSignal(false);
  const [instanceTree, setInstanceTree] = createSignal<FileNode[]>([]);
  const [treeLoading, setTreeLoading] = createSignal(false);
  const [treeError, setTreeError] = createSignal("");
  // D150: two separate selections. The packs picked one by one filter their
  // category; the instance paths are the only thing «Other files» exports.
  const [selectedPaths, setSelectedPaths] = createSignal<Set<string>>(new Set());
  const [selectedPacks, setSelectedPacks] = createSignal<Set<string>>(new Set());
  const otherChecked = () => exportOptions().otherFiles;

  // `ui-modali-8`: what was picked belongs to the mod list it was picked in.
  // Another mod list starts with nothing picked, and its own tree.
  createEffect(on(selectedModListName, () => {
    setSelectedPaths(new Set<string>());
    setSelectedPacks(new Set<string>());
    setInstanceTree([]);
    setTreeError("");
    setExportOptions(options => ({ ...options, selectedOtherPaths: [], selectedContentPaths: [] }));
  }, { defer: true }));

  createEffect(() => {
    const modlist = selectedModListName();
    if (otherChecked() && modlist) {
      setTreeLoading(true);
      setTreeError("");
      invoke("list_instance_files_command", { modlistName: modlist, relativePath: "" })
        .then(tree => { if (selectedModListName() === modlist) setInstanceTree(tree as FileNode[]); })
        .catch(err => {
          if (selectedModListName() !== modlist) return;
          setInstanceTree([]);
          setTreeError(String(err));
        })
        .finally(() => setTreeLoading(false));
    }
  });

  const togglePath = (path: string) => {
    const next = new Set(selectedPaths());
    if (next.has(path)) {
      next.delete(path);
    } else {
      next.add(path);
    }
    setSelectedPaths(next);
    setExportOptions(options => ({ ...options, selectedOtherPaths: [...next] }));
  };

  const categoryFlag = { resourcepacks: "resourcePacks", datapacks: "dataPacks", shaders: "shaders" } as const;

  const togglePack = (path: string) => {
    const next = new Set(selectedPacks());
    const picking = !next.has(path);
    if (picking) {
      next.add(path);
    } else {
      next.delete(path);
    }
    setSelectedPacks(next);
    // Picking a pack means exporting its category: a pick in an unchecked
    // category would otherwise do nothing.
    const flag = categoryFlag[path.split("/")[0] as keyof typeof categoryFlag];
    setExportOptions(options => ({
      ...options,
      ...(picking && flag ? { [flag]: true } : {}),
      selectedContentPaths: [...next],
    }));
  };

  const allContentChecked = () => exportOptions().resourcePacks && exportOptions().dataPacks && exportOptions().shaders;
  const someContentChecked = () => exportOptions().resourcePacks || exportOptions().dataPacks || exportOptions().shaders;

  const toggleAllContent = (checked: boolean) => {
    setExportOptions(options => ({ ...options, resourcePacks: checked, dataPacks: checked, shaders: checked }));
  };

  const handleExport = async () => {
    if (saving()) return;
    setSaving(true);
    try {
      await props.onExport();
    } finally {
      setSaving(false);
    }
  };

  return (
    <Show when={exportModalOpen()}>
      <Modal onClose={() => setExportModalOpen(false)}>
        <ModalHeader title="Export Mod List" description="Choose what to include in the .zip archive" onClose={() => setExportModalOpen(false)} />
        <div class="p-6 space-y-3">
          <label class="flex items-center gap-3 text-sm">
            <input type="checkbox" checked={exportOptions().rulesJson} onChange={e => setExportOptions(options => ({ ...options, rulesJson: e.currentTarget.checked }))} class="h-4 w-4 rounded text-primary" />
            <span class="text-foreground">Mod-list definition (rules.json)</span>
          </label>

          <div>
            <label class="flex items-center gap-3 text-sm">
              <input type="checkbox" checked={exportOptions().configFiles} onChange={e => setExportOptions(options => ({ ...options, configFiles: e.currentTarget.checked }))} class="h-4 w-4 rounded text-primary" />
              <span class="text-foreground">Custom config files</span>
            </label>
            <p class="ml-7 mt-0.5 text-[10px] text-muted-foreground">The files attached to mods under Custom configs. Unchecked, the recipient gets the config entries without their files.</p>
          </div>

          <div>
            <div class="flex items-center gap-2">
              <label class="flex items-center gap-3 text-sm flex-1">
                <input
                  type="checkbox"
                  checked={allContentChecked()}
                  ref={el => { createEffect(() => { el.indeterminate = someContentChecked() && !allContentChecked(); }); }}
                  onChange={e => toggleAllContent(e.currentTarget.checked)}
                  class="h-4 w-4 rounded text-primary"
                />
                <span class="text-foreground">Local content packs</span>
              </label>
              <button onClick={() => setContentOpen(value => !value)} class="text-muted-foreground hover:text-foreground transition-colors p-0.5">
                <Show when={contentOpen()} fallback={<ChevronRightIcon class="h-3.5 w-3.5" />}><ChevronDownIcon class="h-3.5 w-3.5" /></Show>
              </button>
            </div>
            <Show when={contentOpen()}>
              <div class="ml-7 mt-1 space-y-1.5">
                <ContentSubSection
                  label="Resource Packs"
                  contentType="resourcepack"
                  categoryDir="resourcepacks"
                  modlistName={selectedModListName()}
                  checked={exportOptions().resourcePacks}
                  onCheck={value => setExportOptions(options => ({ ...options, resourcePacks: value }))}
                  open={resourcePacksOpen()}
                  onToggleOpen={() => setResourcePacksOpen(value => !value)}
                  selectedPaths={selectedPacks}
                  onTogglePath={togglePack}
                />
                <ContentSubSection
                  label="Data Packs"
                  contentType="datapack"
                  categoryDir="datapacks"
                  modlistName={selectedModListName()}
                  checked={exportOptions().dataPacks}
                  onCheck={value => setExportOptions(options => ({ ...options, dataPacks: value }))}
                  open={dataPacksOpen()}
                  onToggleOpen={() => setDataPacksOpen(value => !value)}
                  selectedPaths={selectedPacks}
                  onTogglePath={togglePack}
                />
                <ContentSubSection
                  label="Shaders"
                  contentType="shader"
                  categoryDir="shaders"
                  modlistName={selectedModListName()}
                  checked={exportOptions().shaders}
                  onCheck={value => setExportOptions(options => ({ ...options, shaders: value }))}
                  open={shadersOpen()}
                  onToggleOpen={() => setShadersOpen(value => !value)}
                  selectedPaths={selectedPacks}
                  onTogglePath={togglePack}
                />
              </div>
            </Show>
          </div>

          <div>
            <label class="flex items-center gap-3 text-sm">
              <input type="checkbox" checked={otherChecked()} onChange={e => { const checked = e.currentTarget.checked; setExportOptions(options => ({ ...options, otherFiles: checked })); }} class="h-4 w-4 rounded text-primary" />
              <span class="text-foreground">Other files from instances</span>
            </label>
            <Show when={otherChecked()}>
              <p class="ml-7 mt-0.5 text-[10px] text-muted-foreground">Only the files and folders you tick below are exported. With nothing ticked, nothing from the instances is.</p>
              <div class="ml-7 mt-2 max-h-48 overflow-y-auto rounded-md border border-border bg-background p-2">
                <Show when={treeLoading()}>
                  <p class="text-xs text-muted-foreground py-2 text-center">Loading instance files...</p>
                </Show>
                <Show when={!treeLoading() && treeError()}>
                  <p class="text-xs text-destructive py-2 text-center break-all">Could not list the instance files: {treeError()}</p>
                </Show>
                <Show when={!treeLoading() && !treeError() && instanceTree().length > 0}>
                  <For each={instanceTree()}>
                    {node => <FileTreeNode node={node} modlistName={selectedModListName()} selected={selectedPaths()} onToggle={togglePath} depth={0} />}
                  </For>
                </Show>
                <Show when={!treeLoading() && !treeError() && instanceTree().length === 0}>
                  <p class="text-xs text-muted-foreground py-2 text-center">No instance files found. Launch the game at least once to create an instance.</p>
                </Show>
                <Show when={selectedPaths().size > 0}>
                  <p class="mt-2 text-[10px] text-muted-foreground border-t border-border pt-1">{selectedPaths().size} path{selectedPaths().size !== 1 ? "s" : ""} selected</p>
                </Show>
              </div>
            </Show>
          </div>

          <p class="pt-2 text-xs text-muted-foreground">
            Always included: the mod list, its resource pack, data pack and shader lists, and the jars of local mods. Mods and Modrinth packs download automatically for the recipient; custom config files, local packs and instance files travel only when checked above.
          </p>
        </div>
        <div class="flex justify-end gap-2 border-t border-border px-6 py-4">
          <button onClick={() => setExportModalOpen(false)} class="rounded-md bg-secondary px-4 py-2 text-sm text-secondary-foreground hover:bg-secondary/80">Cancel</button>
          <button onClick={() => void handleExport()} disabled={saving() || !exportOptions().rulesJson} class="rounded-md bg-primary px-4 py-2 text-sm font-medium text-primary-foreground hover:bg-primary/90 disabled:opacity-50">{saving() ? "Exporting..." : "Export"}</button>
        </div>
      </Modal>
    </Show>
  );
}
