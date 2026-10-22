import { For, Show, createSignal } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { MOD_LOADERS } from "../../lib/types";
import {
  minecraftVersions,
  mcWithSnapshots,
  showSnapshots,
  setShowSnapshots,
} from "../../store";
import { MaterialIcon, XIcon } from "../icons";
import { Select } from "../Select";
import type { ContentEntry } from "./content-types";

interface ContentAdvancedPanelProps {
  entry: ContentEntry;
  name: string;
  modlistName: string;
  contentType: string;
  onClose: () => void;
  onUpdate: (entryId: string, rules: ContentEntry["versionRules"]) => void;
  /** Rilegge la scheda dal disco: dopo un salvataggio rifiutato vale quello. */
  onReload: () => Promise<void>;
}

export function ContentAdvancedPanel(props: ContentAdvancedPanelProps) {
  const allLoaders = ["any", ...MOD_LOADERS];
  const [rules, setRules] = createSignal(props.entry.versionRules);
  const [addingRule, setAddingRule] = createSignal(false);
  const [draftKind, setDraftKind] = createSignal<"exclude" | "only">("exclude");
  const [draftVersions, setDraftVersions] = createSignal<string[]>([]);
  const [draftLoader, setDraftLoader] = createSignal("any");

  const versions = () => showSnapshots() ? mcWithSnapshots() : minecraftVersions();

  // Il pannello copre il banner degli avvisi: un salvataggio rifiutato si dice
  // qui, e le regole tornano a quelle sul disco invece di sembrare salvate
  // (audit 101, § 1 errori ingoiati).
  const [saveError, setSaveError] = createSignal<string | null>(null);

  const save = async (versionRules: ContentEntry["versionRules"]) => {
    // «Any version» è una lista vuota per il backend; una stringa vuota non
    // combacia con nessuna versione (audit 101, ui-schermate-5). Si pulisce
    // anche quello che arriva da file scritti prima della correzione.
    const normalized = versionRules.map(rule => ({ ...rule, mcVersions: rule.mcVersions.filter(version => version !== "") }));
    try {
      await invoke("save_content_version_rules_command", {
        input: {
          modlistName: props.modlistName,
          contentType: props.contentType,
          entryId: props.entry.id,
          versionRules: normalized.map(rule => ({
            kind: rule.kind,
            mc_versions: rule.mcVersions,
            loader: rule.loader,
          })),
        },
      });
      setSaveError(null);
      props.onUpdate(props.entry.id, normalized);
    } catch (error) {
      setSaveError(String(error));
      // Prima le ultime regole confermate, poi quelle vere del disco.
      setRules(props.entry.versionRules);
      await props.onReload();
      const reloaded = props.entry as ContentEntry | null;
      if (reloaded) setRules(reloaded.versionRules);
    }
  };

  const commitRule = () => {
    if (draftVersions().length === 0) return;
    const updated = [...rules(), { kind: draftKind(), mcVersions: draftVersions(), loader: draftLoader() }];
    setRules(updated);
    setAddingRule(false);
    setDraftVersions([]);
    setDraftLoader("any");
    setDraftKind("exclude");
    void save(updated);
  };

  const removeRule = (index: number) => {
    const updated = rules().filter((_, candidateIndex) => candidateIndex !== index);
    setRules(updated);
    void save(updated);
  };

  return (
    <div
      class="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-black/60 px-4 py-8 backdrop-blur-sm"
      onClick={event => {
        if (event.target === event.currentTarget) props.onClose();
      }}
    >
      <div class="flex w-full max-w-2xl flex-col overflow-hidden rounded-lg border border-border bg-card shadow-xl">
        <div class="flex shrink-0 items-center justify-between border-b border-border px-6 py-4">
          <div>
            <h2 class="text-lg font-semibold text-foreground">Advanced</h2>
            <p class="max-w-md truncate text-sm text-muted-foreground">{props.name}</p>
          </div>
          <button onClick={props.onClose} class="flex h-8 w-8 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-muted hover:text-foreground">
            <XIcon class="h-4 w-4" />
          </button>
        </div>
        <div class="max-h-[70vh] flex-1 overflow-y-auto">
          <div class="border-b border-border bg-muted/30 px-5 py-2">
            <h3 class="text-xs font-semibold uppercase tracking-wider text-muted-foreground">Version Rules</h3>
          </div>
          <div class="space-y-2 p-4">
            <Show when={saveError()}>
              <div class="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2" role="alert">
                <p class="text-xs font-semibold text-foreground">Could not save version rules</p>
                <p class="mt-0.5 text-xs text-muted-foreground">The rules shown are the ones saved on disk. {saveError()}</p>
              </div>
            </Show>
            <For each={rules()}>
              {(rule, index) => (
                <div class="flex flex-wrap items-center gap-2 rounded-md border border-border bg-background p-2">
                  <Select
                    value={rule.kind}
                    options={[
                      { value: "exclude", label: "Exclude when" },
                      { value: "only", label: "Only when" },
                    ]}
                    onChange={value => {
                      const updated = rules().map((candidate, candidateIndex) =>
                        candidateIndex === index() ? { ...candidate, kind: value } : candidate
                      );
                      setRules(updated);
                      void save(updated);
                    }}
                    class="rounded border border-border bg-input px-2 py-1 text-xs text-foreground"
                  />
                  <Select
                    value={rule.mcVersions[0] ?? ""}
                    options={[
                      { value: "", label: "Any version" },
                      ...versions().map(version => ({ value: version, label: version })),
                    ]}
                    onChange={value => {
                      const updated = rules().map((candidate, candidateIndex) =>
                        candidateIndex === index() ? { ...candidate, mcVersions: value ? [value] : [] } : candidate
                      );
                      setRules(updated);
                      void save(updated);
                    }}
                    class="rounded border border-border bg-input px-2 py-1 text-xs text-foreground"
                  />
                  <Select
                    value={rule.loader}
                    options={allLoaders.map(loader => ({
                      value: loader,
                      label: loader === "any" ? "Any loader" : loader,
                    }))}
                    onChange={value => {
                      const updated = rules().map((candidate, candidateIndex) =>
                        candidateIndex === index() ? { ...candidate, loader: value } : candidate
                      );
                      setRules(updated);
                      void save(updated);
                    }}
                    class="rounded border border-border bg-input px-2 py-1 text-xs text-foreground"
                  />
                  <button onClick={() => removeRule(index())} class="ml-auto flex h-6 w-6 items-center justify-center rounded text-muted-foreground hover:bg-destructive/10 hover:text-destructive">
                    <XIcon class="h-3.5 w-3.5" />
                  </button>
                </div>
              )}
            </For>

            <Show
              when={addingRule()}
              fallback={
                <button onClick={() => setAddingRule(true)} class="flex items-center gap-1.5 text-xs text-primary transition-colors hover:text-primary/80">
                  <MaterialIcon name="add" size="sm" />
                  Add Version Rule
                </button>
              }
            >
              <div class="space-y-2 rounded-md border border-primary/30 bg-primary/5 p-3">
                <div class="flex flex-wrap items-center gap-2">
                  <Select
                    value={draftKind()}
                    options={[
                      { value: "exclude", label: "Exclude when" },
                      { value: "only", label: "Only when" },
                    ]}
                    onChange={value => setDraftKind(value as "exclude" | "only")}
                    class="rounded border border-border bg-input px-2 py-1 text-xs text-foreground"
                  />
                  <Select
                    value={draftVersions()[0] ?? ""}
                    options={[
                      { value: "", label: "Select version..." },
                      ...versions().map(version => ({ value: version, label: version })),
                    ]}
                    onChange={value => setDraftVersions(value ? [value] : [])}
                    class="rounded border border-border bg-input px-2 py-1 text-xs text-foreground"
                  />
                  <Select
                    value={draftLoader()}
                    options={allLoaders.map(loader => ({
                      value: loader,
                      label: loader === "any" ? "Any loader" : loader,
                    }))}
                    onChange={value => setDraftLoader(value)}
                    class="rounded border border-border bg-input px-2 py-1 text-xs text-foreground"
                  />
                </div>
                <div class="flex items-center gap-2">
                  <label class="cursor-pointer flex items-center gap-1.5 text-xs text-muted-foreground">
                    <input type="checkbox" checked={showSnapshots()} onChange={event => setShowSnapshots(event.currentTarget.checked)} class="rounded border-border" />
                    Show Snapshots
                  </label>
                </div>
                <div class="flex gap-2">
                  <button onClick={commitRule} disabled={draftVersions().length === 0} class="rounded-md bg-primary px-3 py-1 text-xs text-white disabled:opacity-50">Add</button>
                  <button onClick={() => setAddingRule(false)} class="rounded-md bg-secondary px-3 py-1 text-xs text-secondary-foreground">Cancel</button>
                </div>
              </div>
            </Show>
          </div>
        </div>
      </div>
    </div>
  );
}
