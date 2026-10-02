import { For } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";
import { customConfigs, addCustomConfig, removeCustomConfig, updateCustomConfig, pushUiError, rowMap, selectedModListName } from "../../store";
import { minecraftVersions } from "../../store";
import { MaterialIcon, XIcon } from "../icons";
import { Select } from "../Select";
import { ALL_LOADERS, isTauri } from "./shared";

export function CustomConfigsSection(props: { modId: string }) {
  const myConfigs = () => customConfigs().filter(config => config.modId === props.modId);

  // D112: the picked files are copied into the mod list by the backend, and
  // `files` keeps the names it returns — never a path on the user's disk.
  const pickFiles = async (configId: string) => {
    if (!isTauri()) return;
    const modlistName = selectedModListName();
    if (!modlistName) return;
    let result: string | string[] | null;
    try {
      result = await openFileDialog({ multiple: true, directory: false });
    } catch {
      return; // dialog cancelled
    }
    if (!result) return;
    const sourcePaths = Array.isArray(result) ? result : [result];
    const cfg = customConfigs().find(config => config.id === configId);
    if (!cfg) return;
    try {
      const names = await invoke<string[]>("import_custom_config_files_command", {
        input: {
          modlistName,
          modId: rowMap().get(cfg.modId)?.primaryModId ?? cfg.modId,
          configId,
          sourcePaths,
          currentFiles: cfg.files,
        },
      });
      const current = customConfigs().find(config => config.id === configId);
      if (!current) return;
      const added = names.filter(name => !current.files.includes(name));
      if (added.length > 0) updateCustomConfig(configId, { files: [...current.files, ...added] });
    } catch (error) {
      pushUiError({ title: "Could not add config files", message: "The selected files were not copied into the mod list.", detail: String(error), severity: "error", scope: "launch" });
    }
  };

  return (
    <div class="border-t border-border p-4 space-y-3 shrink-0 max-h-64 overflow-y-auto">
      <For each={myConfigs()}>
        {cfg => (
          <div class="rounded-md border border-border bg-background p-3 space-y-2">
            <div class="flex items-center justify-between">
              <span class="text-xs font-semibold text-muted-foreground uppercase tracking-wide">Config</span>
              <button
                onClick={() => removeCustomConfig(cfg.id)}
                class="flex items-center gap-1 rounded px-2 py-0.5 text-xs text-destructive hover:bg-destructive/10 transition-colors"
              >
                <XIcon class="h-3 w-3" /> Delete
              </button>
            </div>

            <div class="flex items-center gap-2">
              <label class="text-xs text-muted-foreground shrink-0">Versions:</label>
              <Select
                value={cfg.mcVersions[0] ?? ""}
                options={[
                  { value: "", label: "Any version" },
                  ...minecraftVersions().map(version => ({ value: version, label: version })),
                ]}
                onChange={value => updateCustomConfig(cfg.id, { mcVersions: value ? [value] : [] })}
                class="rounded border border-border bg-input px-2 py-1 text-xs text-foreground flex-1"
              />
            </div>

            <div class="flex items-center gap-2">
              <label class="text-xs text-muted-foreground shrink-0">Loader:</label>
              <Select
                value={cfg.loader}
                options={ALL_LOADERS.map(loader => ({ value: loader, label: loader === "any" ? "Any loader" : loader }))}
                onChange={value => updateCustomConfig(cfg.id, { loader: value })}
                class="rounded border border-border bg-input px-2 py-1 text-xs text-foreground"
              />
            </div>

            <div class="flex items-center gap-2">
              <label class="text-xs text-muted-foreground shrink-0">Path:</label>
              <input
                type="text"
                value={cfg.targetPath}
                onInput={e => updateCustomConfig(cfg.id, { targetPath: e.currentTarget.value })}
                placeholder="e.g. config or shaderpacks"
                class="flex-1 rounded border border-border bg-input px-2 py-1 text-xs text-foreground placeholder-muted-foreground focus:outline-none focus:ring-1 focus:ring-ring"
              />
            </div>

            <div>
              <label class="text-xs text-muted-foreground mb-1 block">Config files:</label>
              <div class="space-y-1">
                <For each={cfg.files}>
                  {(file, idx) => (
                    <div class="flex items-center gap-2 rounded bg-muted/30 px-2 py-1">
                      <span class="flex-1 truncate text-xs text-foreground" title={file}>{file}</span>
                      <button
                        onClick={() => updateCustomConfig(cfg.id, { files: cfg.files.filter((_, index) => index !== idx()) })}
                        class="shrink-0 text-muted-foreground hover:text-destructive transition-colors"
                      >
                        <XIcon class="h-3 w-3" />
                      </button>
                    </div>
                  )}
                </For>
              </div>
              <button
                onClick={() => void pickFiles(cfg.id)}
                class="mt-1.5 flex items-center gap-1 rounded-md border border-dashed border-border px-3 py-1.5 text-xs text-muted-foreground hover:border-primary/50 hover:text-foreground transition-colors w-full justify-center"
              >
                <MaterialIcon name="upload_file" size="sm" />
                Add Files...
              </button>
            </div>
          </div>
        )}
      </For>

      <button
        onClick={() => addCustomConfig(props.modId)}
        class="flex w-full items-center justify-center gap-1.5 rounded-md border border-dashed border-border px-4 py-2 text-sm text-muted-foreground hover:border-primary/50 hover:text-foreground transition-colors"
      >
        <MaterialIcon name="add" size="md" />
        Add Custom Config
      </button>
    </div>
  );
}
