import { For, Show, createEffect, createSignal } from "solid-js";
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
      } catch {
        setChildren([]);
      }
      setLoaded(true);
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
        <Show when={loaded() && children().length === 0}>
          <div style={{ "padding-left": `${(depth() + 1) * 16}px` }} class="text-[10px] text-muted-foreground py-0.5 px-1">Empty</div>
        </Show>
      </Show>
    </div>
  );
}

function ContentSubSection(subProps: {
  label: string;
  dirName: string;
  checked: boolean;
  onCheck: (value: boolean) => void;
  open: boolean;
  onToggleOpen: () => void;
  selectedPaths: () => Set<string>;
  onTogglePath: (path: string) => void;
}) {
  const [files, setFiles] = createSignal<{ name: string; path: string }[]>([]);
  const [filesLoaded, setFilesLoaded] = createSignal(false);

  createEffect(() => {
    if (subProps.open && !filesLoaded() && selectedModListName()) {
      const modlist = selectedModListName();
      void (async () => {
        const roots = await invoke("list_instance_files_command", { modlistName: modlist, relativePath: "" }).catch(() => [] as FileNode[]) as FileNode[];
        const allFiles: { name: string; path: string }[] = [];
        for (const instance of roots) {
          if (!instance.isDir) continue;
          try {
            const children = await invoke("list_instance_files_command", { modlistName: modlist, relativePath: `${instance.path}/${subProps.dirName}` }) as FileNode[];
            for (const file of children) {
              allFiles.push({ name: `${instance.name}/${file.name}`, path: file.path });
            }
          } catch {
            // directory may not exist
          }
        }
        setFiles(allFiles);
        setFilesLoaded(true);
      })();
    }
  });

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
          <Show when={files().length > 0} fallback={<p class="text-[10px] text-muted-foreground py-1">{filesLoaded() ? "None found" : "Loading..."}</p>}>
            <For each={files()}>
              {file => (
                <label class="flex items-center gap-2 text-xs py-0.5 cursor-pointer hover:bg-muted/30 rounded px-1">
                  <input
                    type="checkbox"
                    checked={subProps.selectedPaths().has(file.path)}
                    onChange={() => subProps.onTogglePath(file.path)}
                    class="h-3 w-3 rounded text-primary"
                  />
                  <MaterialIcon name="description" size="sm" class="text-muted-foreground" />
                  <span class="text-muted-foreground truncate">{file.name}</span>
                </label>
              )}
            </For>
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
                      <Show when={err.action}>
                        {action => (
                          <button
                            onClick={() => { setErrorCenterOpen(false); action().run(); }}
                            class="mt-2 text-xs font-medium text-primary underline underline-offset-2 hover:text-foreground"
                          >
                            {action().label}
                          </button>
                        )}
                      </Show>
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
  const [otherChecked, setOtherChecked] = createSignal(false);
  const [instanceTree, setInstanceTree] = createSignal<FileNode[]>([]);
  const [treeLoading, setTreeLoading] = createSignal(false);
  const [selectedPaths, setSelectedPaths] = createSignal<Set<string>>(new Set());

  createEffect(() => {
    if (otherChecked() && selectedModListName()) {
      setTreeLoading(true);
      invoke("list_instance_files_command", { modlistName: selectedModListName(), relativePath: "" })
        .then(tree => setInstanceTree(tree as FileNode[]))
        .catch(() => setInstanceTree([]))
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

          <label class="flex items-center gap-3 text-sm">
            <input type="checkbox" checked={exportOptions().configFiles} onChange={e => setExportOptions(options => ({ ...options, configFiles: e.currentTarget.checked }))} class="h-4 w-4 rounded text-primary" />
            <span class="text-foreground">Config files</span>
          </label>

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
                <span class="text-foreground">Content packs</span>
              </label>
              <button onClick={() => setContentOpen(value => !value)} class="text-muted-foreground hover:text-foreground transition-colors p-0.5">
                <Show when={contentOpen()} fallback={<ChevronRightIcon class="h-3.5 w-3.5" />}><ChevronDownIcon class="h-3.5 w-3.5" /></Show>
              </button>
            </div>
            <Show when={contentOpen()}>
              <div class="ml-7 mt-1 space-y-1.5">
                <ContentSubSection
                  label="Resource Packs"
                  dirName="resourcepacks"
                  checked={exportOptions().resourcePacks}
                  onCheck={value => setExportOptions(options => ({ ...options, resourcePacks: value }))}
                  open={resourcePacksOpen()}
                  onToggleOpen={() => setResourcePacksOpen(value => !value)}
                  selectedPaths={selectedPaths}
                  onTogglePath={togglePath}
                />
                <ContentSubSection
                  label="Data Packs"
                  dirName="datapacks"
                  checked={exportOptions().dataPacks}
                  onCheck={value => setExportOptions(options => ({ ...options, dataPacks: value }))}
                  open={dataPacksOpen()}
                  onToggleOpen={() => setDataPacksOpen(value => !value)}
                  selectedPaths={selectedPaths}
                  onTogglePath={togglePath}
                />
                <ContentSubSection
                  label="Shaders"
                  dirName="shaderpacks"
                  checked={exportOptions().shaders}
                  onCheck={value => setExportOptions(options => ({ ...options, shaders: value }))}
                  open={shadersOpen()}
                  onToggleOpen={() => setShadersOpen(value => !value)}
                  selectedPaths={selectedPaths}
                  onTogglePath={togglePath}
                />
              </div>
            </Show>
          </div>

          <div>
            <label class="flex items-center gap-3 text-sm">
              <input type="checkbox" checked={otherChecked()} onChange={e => { setOtherChecked(e.currentTarget.checked); setExportOptions(options => ({ ...options, otherFiles: e.currentTarget.checked })); }} class="h-4 w-4 rounded text-primary" />
              <span class="text-foreground">Other files from instances</span>
            </label>
            <Show when={otherChecked()}>
              <div class="ml-7 mt-2 max-h-48 overflow-y-auto rounded-md border border-border bg-background p-2">
                <Show when={treeLoading()}>
                  <p class="text-xs text-muted-foreground py-2 text-center">Loading instance files...</p>
                </Show>
                <Show when={!treeLoading() && instanceTree().length > 0}>
                  <For each={instanceTree()}>
                    {node => <FileTreeNode node={node} modlistName={selectedModListName()} selected={selectedPaths()} onToggle={togglePath} depth={0} />}
                  </For>
                </Show>
                <Show when={!treeLoading() && instanceTree().length === 0}>
                  <p class="text-xs text-muted-foreground py-2 text-center">No instance files found. Launch the game at least once to create an instance.</p>
                </Show>
                <Show when={selectedPaths().size > 0}>
                  <p class="mt-2 text-[10px] text-muted-foreground border-t border-border pt-1">{selectedPaths().size} path{selectedPaths().size !== 1 ? "s" : ""} selected</p>
                </Show>
              </div>
            </Show>
          </div>

          <p class="pt-2 text-xs text-muted-foreground">
            Exports the mod list and the jars that cannot be re-downloaded. Everything else the recipient's Cubic Launcher downloads automatically.
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
