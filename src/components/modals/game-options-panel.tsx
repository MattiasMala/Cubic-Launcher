import { invoke } from "@tauri-apps/api/core";
import { For, Show, createMemo, createSignal } from "solid-js";
import { MaterialIcon } from "../icons";
import { Select } from "../Select";

/**
 * Le impostazioni di gioco condivise, dentro i Settings della modlist.
 *
 * Quello che si vede qui viene **dal file** (D66): `options.txt` della modlist
 * si apre anche con un editor di testo, e se Mattias lo modifica lì la GUI
 * deve mostrare quello che c'è scritto, non una copia tenuta altrove.
 */

export type OptionsScope =
  | { level: "global" }
  | { level: "modlist"; modlist: string }
  | { level: "instance"; modlist: string; instance: string };

export type OptionKind = "plain" | "keybind" | "soundCategory" | "modelPart";

export type OptionEntryView = {
  key: string;
  value: string;
  kind: OptionKind;
  vanilla: boolean;
  internalState: boolean;
  group?: string | null;
};

export type SharedOptionsView = {
  path: string;
  exists: boolean;
  writable: boolean;
  dataVersion?: number | null;
  versionId?: string | null;
  derivationError?: string | null;
  entries: OptionEntryView[];
};

export type BlockReason = "notVanilla" | "internalState" | "resourcePacksOff" | "unchecked";

export type SeedStatus =
  | {
      status: "seeded";
      report: {
        source: string;
        target: string;
        dataVersion: number;
        sourceVersionId: string;
        seeded: number;
        blocked: Array<{ key: string; reason: BlockReason }>;
      };
    }
  | { status: "skippedTargetExists"; target: string }
  | { status: "skippedNoSource"; source: string }
  | { status: "refused"; reason: string };

export type Invoke = (command: string, args?: Record<string, unknown>) => Promise<unknown>;

const RESOURCE_PACK_KEYS = ["resourcePacks", "incompatibleResourcePacks"];

const PREFIX_GROUP_LABELS: Record<Exclude<OptionKind, "plain">, string> = {
  keybind: "Controls",
  soundCategory: "Music & Sounds",
  modelPart: "Skin Customization",
};

/**
 * Il gruppo di ripiego. Su 1.20.1 è l'unico che esiste per le impostazioni
 * semplici — il jar è offuscato e i nomi delle schermate non sopravvivono —
 * quindi deve reggere da solo 86 righe: per questo la casella di ricerca sopra
 * l'elenco filtra su tutti i gruppi e questo resta ordinato per nome.
 */
const FALLBACK_GROUP = "Other settings";

const REASON_LABELS: Record<BlockReason, string> = {
  notVanilla: "not a vanilla key (almost always from a mod)",
  internalState: "state of that installation, not a preference",
  resourcePacksOff: "the resource pack list, off for this hop",
  unchecked: "unchecked",
};


/**
 * Il livello che il pannello apre. Le istanze non sono qui: si guardano dal
 * menu «From» di una modlist e non si modificano (D73).
 */
export type PanelScope = { level: "global" } | { level: "modlist"; modlist: string };

interface Props {
  scope: PanelScope;
  /** Iniettabile per poter mostrare il pannello fuori da Tauri. */
  invoke?: Invoke;
  /** Vista iniziale, quando il chiamante l'ha già caricata. */
  initialView?: SharedOptionsView;
  initialInstances?: string[];
}

export function GameOptionsPanel(props: Props) {
  const call = (): Invoke => props.invoke ?? invoke;

  const [view, setView] = createSignal<SharedOptionsView | null>(props.initialView ?? null);
  const [instances, setInstances] = createSignal<string[]>(props.initialInstances ?? []);
  const [loading, setLoading] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [notice, setNotice] = createSignal<string | null>(null);

  const [search, setSearch] = createSignal("");
  /**
   * Spunta esplicita dell'utente, dove l'ha messa. Quello che non è qui dentro
   * segue il default del salto: è così che il default di D67 sui resource pack
   * resta **ribaltabile** invece di essere cablato nella direzione.
   */
  const [checkedOverride, setCheckedOverride] = createSignal<Record<string, boolean>>({});
  const [collapsed, setCollapsed] = createSignal<Set<string>>(new Set());
  const [blockedOpen, setBlockedOpen] = createSignal(false);
  const [edits, setEdits] = createSignal<Record<string, string>>({});

  const [source, setSource] = createSignal("default");
  const [instance, setInstance] = createSignal("");
  /**
   * Verso dove si sta per seminare, che decide i default delle spunte (D67).
   * Dal globale l'unico salto possibile è verso una modlist — istanze sotto di
   * sé non ne ha — quindi lì parte di lì e il selettore non si mostra: se
   * restasse su «istanza», il contatore userebbe i default di un salto che da
   * quel file non si può fare, e direbbe che i resource pack passano.
   */
  const [jump, setJump] = createSignal<"instance" | "modlist">(
    props.scope.level === "global" ? "modlist" : "instance",
  );
  const [confirm, setConfirm] = createSignal<null | {
    title: string;
    detail: string;
    run: () => Promise<void>;
  }>(null);

  /**
   * Il caso che il backend rifiuta: il file c'è ma qualcuno ha cancellato la
   * riga `version:` con un editor. Inventare una DataVersion sarebbe l'unica
   * cosa peggiore del rifiuto, quindi qui si dice cosa manca e come rimetterla.
   */
  const missingVersion = () => {
    const current = view();
    return Boolean(current?.exists) && (current?.dataVersion ?? null) === null;
  };
  /** Il nome della modlist, quando il pannello ne sta aprendo una. */
  const modlistName = () => (props.scope.level === "modlist" ? props.scope.modlist : null);

  /**
   * Quale file si sta guardando. `default` è il file di questo livello; le
   * altre voci — le istanze di questa modlist, e solo quelle (D74) — **non
   * copiano niente**, aprono quel file in lettura. Così le spunte e il
   * contatore descrivono la sorgente vera di quel salto, che è quello che
   * chiede D67, e per portarsela qui serve il bottone apposta, che chiede
   * conferma.
   *
   * Le altre modlist non compaiono: copiare da una modlist all'altra è fuori
   * dalla feature, e la voce è stata tolta, non nascosta.
   */
  const previewing = () => source() !== "default";
  const sourceScope = (): OptionsScope => {
    const selected = source();
    const modlist = modlistName();
    if (selected.startsWith("i:") && modlist !== null) {
      return { level: "instance", modlist, instance: selected.slice(2) };
    }
    return props.scope;
  };
  const sourceLabel = () => {
    const selected = source();
    if (selected.startsWith("i:")) return `instance ${selected.slice(2)}`;
    return "default";
  };

  const load = async () => {
    setLoading(true);
    setError(null);
    try {
      const loaded = (await call()("load_shared_options_command", {
        scope: sourceScope(),
      })) as SharedOptionsView;
      setView(loaded);
      setEdits({});
      // Il globale non ha istanze: niente da elencare, e nessun menu «From».
      const modlist = modlistName();
      if (modlist === null) {
        setInstances([]);
        return;
      }
      const listed = (await call()("list_instance_files_command", {
        modlistName: modlist,
      })) as Array<{ name: string; isDir: boolean }>;
      setInstances(listed.filter(node => node.isDir).map(node => node.name));
    } catch (loadError) {
      setError(String(loadError));
    } finally {
      setLoading(false);
    }
  };

  if (!props.initialView) void load();

  /**
   * Lo stato interno non si mostra (D72): è vanilla ma non è una preferenza.
   * E nemmeno `version`, che non è un'impostazione: la semina la riscrive
   * comunque, quindi una spunta lì sarebbe finta e un campo di testo sarebbe
   * un modo di falsificare la DataVersion. Sta nel distintivo in alto, in sola
   * lettura, e resta nel file perché il salvataggio rimanda tutte le righe.
   */
  const visible = createMemo(() =>
    (view()?.entries ?? []).filter(entry => !entry.internalState && entry.key !== "version"),
  );

  const matches = (entry: OptionEntryView) => {
    const needle = search().trim().toLowerCase();
    if (!needle) return true;
    return entry.key.toLowerCase().includes(needle) || entry.value.toLowerCase().includes(needle);
  };

  const groupOf = (entry: OptionEntryView): string => {
    if (entry.kind !== "plain") return PREFIX_GROUP_LABELS[entry.kind];
    // `VideoSettingsScreen` → `Video Settings`: nessuna tabella da mantenere.
    return entry.group
      ? entry.group.replace(/Screen$/, "").replace(/([a-z])([A-Z])/g, "$1 $2").trim()
      : FALLBACK_GROUP;
  };

  const groups = createMemo(() => {
    const buckets = new Map<string, OptionEntryView[]>();
    for (const entry of visible()) {
      if (!matches(entry)) continue;
      const name = groupOf(entry);
      const bucket = buckets.get(name);
      if (bucket) bucket.push(entry);
      else buckets.set(name, [entry]);
    }

    const prefixOrder = Object.values(PREFIX_GROUP_LABELS);
    const rank = (name: string) => {
      if (name === FALLBACK_GROUP) return 1;
      const prefixIndex = prefixOrder.indexOf(name);
      return prefixIndex >= 0 ? 2 + prefixIndex : 0;
    };

    return [...buckets.entries()]
      .map(([name, entries]) => ({ name, entries: entries.slice().sort((a, b) => a.key.localeCompare(b.key)) }))
      .sort((a, b) => rank(a.name) - rank(b.name) || a.name.localeCompare(b.name));
  });

  /**
   * Il default della spunta cambia con il salto (D67): la lista dei resource
   * pack parte spenta verso una modlist o il globale, accesa verso un'istanza
   * della stessa modlist. L'utente può ribaltarlo riga per riga, e la sua
   * scelta vince.
   *
   * `toInstance` è il salto vero di quel gesto, non quello scelto nel
   * selettore: il selettore è solo l'anteprima del contatore, e il bottone
   * premuto può andare da un'altra parte.
   */
  const isChecked = (key: string, toInstance = jump() === "instance") => {
    const override = checkedOverride()[key];
    if (override !== undefined) return override;
    return !(RESOURCE_PACK_KEYS.includes(key) && !toInstance);
  };

  const blocked = createMemo(() => {
    const rows: Array<{ key: string; reason: BlockReason }> = [];
    for (const entry of view()?.entries ?? []) {
      if (entry.key === "version") continue;
      if (entry.internalState) rows.push({ key: entry.key, reason: "internalState" });
      else if (!entry.vanilla) rows.push({ key: entry.key, reason: "notVanilla" });
      else if (!isChecked(entry.key)) {
        rows.push({
          key: entry.key,
          reason:
            checkedOverride()[entry.key] === undefined ? "resourcePacksOff" : "unchecked",
        });
      }
    }
    return rows;
  });

  /** Le righe che quel salto non copia, nella forma che vuole il backend. */
  const uncheckedKeys = (toInstance: boolean) =>
    (view()?.entries ?? [])
      .filter(entry => !isChecked(entry.key, toInstance))
      .map(entry => entry.key);

  const toggle = (key: string) => {
    setCheckedOverride(current => ({ ...current, [key]: !isChecked(key) }));
  };

  const toggleGroup = (name: string) => {
    setCollapsed(current => {
      const next = new Set(current);
      if (next.has(name)) next.delete(name);
      else next.add(name);
      return next;
    });
  };

  const describe = (status: SeedStatus): string => {
    if (status.status === "seeded") {
      return `${status.report.seeded} settings written to ${status.report.target}; ${status.report.blocked.length} did not pass.`;
    }
    if (status.status === "refused") return `Refused: ${status.reason}`;
    if (status.status === "skippedNoSource") return `Nothing to copy: ${status.source} does not exist.`;
    return `Nothing to do: ${status.target} already exists.`;
  };

  const apply = async (from: OptionsScope, to: OptionsScope, toInstance: boolean) => {
    setNotice(null);
    setError(null);
    try {
      const status = (await call()("apply_options_command", {
        from,
        to,
        unchecked: uncheckedKeys(toInstance),
        // Le spunte governano anche i pack: qui si dice sempre di sì, e chi
        // resta fuori è già in `unchecked`. Così il default di D67 resta un
        // default e non una regola cablata nella direzione.
        includeResourcePacks: true,
      })) as SeedStatus;
      setNotice(describe(status));
      await load();
    } catch (applyError) {
      setError(String(applyError));
    }
  };

  const save = async () => {
    setNotice(null);
    setError(null);
    const current = view();
    if (!current) return;
    // Si rimandano **tutte** le righe, comprese quelle che non mostriamo:
    // salvare solo le visibili cancellerebbe lo stato interno dal file.
    const entries = current.entries.map(entry => ({
      key: entry.key,
      value: edits()[entry.key] ?? entry.value,
    }));
    try {
      await call()("save_shared_options_command", { scope: props.scope, entries });
      setNotice("Saved.");
      await load();
    } catch (saveError) {
      setError(String(saveError));
    }
  };

  const onSourceChange = (value: string) => {
    // Il Select in-DOM lascia cliccare la voce già scelta: ricaricare lì
    // butterebbe via modifiche e spunte senza che sia cambiato niente.
    if (value === source()) return;
    setSource(value);
    setCheckedOverride({});
    // In anteprima l'unico gesto è "porta qui", cioè un salto verso questo
    // livello; tornando a «default» il gesto disponibile è di nuovo la
    // ri-applicazione a un'istanza. Il selettore del contatore segue, se no
    // le spunte mostrano un default e il bottone ne usa un altro.
    setJump(value === "default" ? "instance" : "modlist");
    setNotice(
      value === "default" ? null : `Viewing ${sourceLabel()}. Nothing has been copied.`,
    );
    void load();
  };

  const copyFromSource = () => {
    if (!previewing()) return;
    setConfirm({
      title: `Bring the settings from ${sourceLabel()} here?`,
      detail: `Overwrites mod-lists/${modlistName()}/options.txt, the default that new instances are born from. Instances that already exist do not change.`,
      run: async () => {
        await apply(sourceScope(), props.scope, false);
        setSource("default");
        setCheckedOverride({});
        await load();
      },
    });
  };

  const reapply = () => {
    const target = instance();
    const modlist = modlistName();
    if (!target || modlist === null) return;
    setConfirm({
      title: `Re-apply to “${target}”?`,
      detail: `Overwrites mod-lists/${modlist}/instances/${target}/options.txt, which has belonged to the game since it was created: whatever you changed in-game in that instance is lost.`,
      run: () => apply(props.scope, { level: "instance", modlist, instance: target }, true),
    });
  };

  const promoteToGlobal = () => {
    setConfirm({
      title: "Promote to the global default?",
      detail: "Overwrites the global options.txt, the template for new mod-lists. Mod-lists that already exist do not change.",
      run: () => apply(props.scope, { level: "global" }, false),
    });
  };

  return (
    <div class="flex flex-col gap-3" data-testid="game-options-panel">
      <div class="flex flex-wrap items-center gap-2 rounded-md border border-border bg-background px-3 py-2 text-xs">
        <MaterialIcon name="description" size="sm" class="text-muted-foreground" />
        <code class="truncate text-muted-foreground">{view()?.path ?? "—"}</code>
        <Show when={view()?.versionId}>
          <span class="rounded-full bg-muted px-2 py-0.5 text-[10px] text-muted-foreground">
            version:{view()!.dataVersion} · {view()!.versionId}
          </span>
        </Show>
        {/*
          Il menu vive solo dove ha senso: il globale non ha istanze sotto di
          sé, e le altre modlist non sono una sorgente (D74).
        */}
        <Show when={modlistName() !== null}>
          <div class="ml-auto flex min-w-0 items-center gap-2">
            <span class="text-muted-foreground">From</span>
            <Select
              value={source()}
              onChange={onSourceChange}
              align="right"
              // Il pannello sta dentro un modale: il menu si apre verso
              // sinistra e si tronca, invece di uscire dal bordo destro.
              panelClass="max-w-[18rem]"
              class="max-w-[14rem] rounded-md border border-input bg-input px-2 py-1 text-xs text-foreground"
              options={[
                { value: "default", label: "default" },
                ...instances().map(name => ({ value: `i:${name}`, label: `instance ${name}` })),
              ]}
              title="View an instance's settings without copying them"
            />
          </div>
        </Show>
      </div>

      <Show when={view() && !view()!.exists}>
        <p class="rounded-md border border-border bg-muted/30 px-3 py-2 text-xs text-muted-foreground">
          <Show
            when={modlistName() !== null}
            fallback={
              <>
                There is no global <code>options.txt</code> yet. It is written the first
                time you promote a mod-list's settings to the global default.
              </>
            }
          >
            <>
              This mod-list has no <code>options.txt</code> yet. It is born from the global
              file when the mod-list is created, or promoted from an instance below.
            </>
          </Show>
        </p>
      </Show>

      <Show when={missingVersion()}>
        <p
          class="rounded-md border border-red-700/40 bg-red-900/20 px-3 py-2 text-xs text-red-300"
          data-testid="missing-version"
        >
          This file has no <code>version:</code> line, and without it the game treats it as
          DataVersion 0: on the first read it would apply every options datafixer, including
          the one that remaps key codes. I will not invent one, and do not take it from a
          random instance: that number says who wrote <em>these</em> values, and pasting a
          different one migrates them wrong.
          <Show
            when={modlistName() !== null}
            fallback={<> Put the line that was there back. Until then, saving is disabled.</>}
          >
            <>
              {" "}Either put the line that was there back, or pick an instance in the “From”
              menu above and press “Bring these settings”, which brings the file and the
              number together. Until then, saving is disabled.
            </>
          </Show>
        </p>
      </Show>

      <Show when={view()?.derivationError && !missingVersion()}>
        <p class="rounded-md border border-amber-700/40 bg-amber-900/20 px-3 py-2 text-xs text-amber-300">
          I can't tell which keys are vanilla: {view()!.derivationError}. While that is the
          case, nothing is seeded.
        </p>
      </Show>

      <Show when={error()}>
        <p class="rounded-md border border-red-700/40 bg-red-900/20 px-3 py-2 text-xs text-red-300">{error()}</p>
      </Show>
      <Show when={notice()}>
        <p class="rounded-md border border-border bg-muted/30 px-3 py-2 text-xs text-muted-foreground">{notice()}</p>
      </Show>

      <div class="flex items-center gap-2">
        <input
          type="text"
          value={search()}
          onInput={event => setSearch(event.currentTarget.value)}
          placeholder="Search settings…"
          class="w-full rounded-md border border-input bg-input px-3 py-1.5 text-sm text-foreground focus:outline-none focus:ring-1 focus:ring-ring"
        />
      </div>

      <div class="max-h-[46vh] overflow-y-auto rounded-md border border-border">
        <Show
          when={!loading()}
          fallback={<p class="px-3 py-6 text-center text-xs text-muted-foreground">Loading…</p>}
        >
          <For each={groups()}>
            {group => (
              <div class="border-b border-border last:border-b-0">
                <button
                  type="button"
                  onClick={() => toggleGroup(group.name)}
                  class="flex w-full items-center gap-2 bg-muted/30 px-3 py-1.5 text-left text-xs font-medium text-foreground hover:bg-muted/60"
                >
                  <MaterialIcon
                    name={collapsed().has(group.name) ? "chevron_right" : "expand_more"}
                    size="sm"
                    class="opacity-60"
                  />
                  {group.name}
                  <span class="text-muted-foreground">{group.entries.length}</span>
                </button>
                <Show when={!collapsed().has(group.name)}>
                  <For each={group.entries}>
                    {entry => (
                      <div class="flex items-center gap-2 px-3 py-1 text-xs">
                        <input
                          type="checkbox"
                          checked={entry.vanilla && isChecked(entry.key)}
                          disabled={!entry.vanilla}
                          onChange={() => toggle(entry.key)}
                          title={
                            entry.vanilla
                              ? "Copy this line when seeding"
                              : "Not vanilla: never seeded"
                          }
                        />
                        <code
                          class={`w-1/2 truncate ${entry.vanilla ? "text-foreground" : "text-muted-foreground line-through"}`}
                        >
                          {entry.key}
                        </code>
                        <input
                          type="text"
                          value={edits()[entry.key] ?? entry.value}
                          readOnly={previewing()}
                          title={
                            previewing()
                              ? "An instance's options.txt belongs to the game: you can look at it and promote it, not edit it"
                              : undefined
                          }
                          onInput={event =>
                            setEdits(current => ({ ...current, [entry.key]: event.currentTarget.value }))
                          }
                          class="flex-1 rounded border border-input bg-input px-2 py-0.5 text-xs text-foreground read-only:opacity-60 focus:outline-none focus:ring-1 focus:ring-ring"
                        />
                      </div>
                    )}
                  </For>
                </Show>
              </div>
            )}
          </For>
        </Show>
      </div>

      <div class="rounded-md border border-border">
        <div class="flex items-center gap-2 px-3 py-1.5 text-xs">
          <button
            type="button"
            onClick={() => setBlockedOpen(open => !open)}
            class="flex items-center gap-2 text-left text-muted-foreground hover:text-foreground"
            data-testid="blocked-counter"
          >
            <MaterialIcon name={blockedOpen() ? "expand_more" : "chevron_right"} size="sm" />
            {blocked().length === 1 ? "1 line doesn't pass" : `${blocked().length} lines don't pass`}
          </button>
          {/*
            Il globale non ha istanze sotto di sé: il suo unico salto è verso
            una modlist, e un selettore con una voce sola sarebbe finto.
          */}
          <Show when={modlistName() !== null}>
            <div class="ml-auto flex items-center gap-2">
              <span class="text-muted-foreground">seeding toward</span>
              <Select
                value={jump()}
                onChange={value => setJump(value as "instance" | "modlist")}
                align="right"
                panelClass="max-w-[18rem]"
                class="max-w-[14rem] rounded-md border border-input bg-input px-2 py-0.5 text-xs text-foreground"
                options={[
                  { value: "instance", label: "an instance of this mod-list" },
                  { value: "modlist", label: "the global default" },
                ]}
                title="The defaults change with the hop (D67)"
                disabled={previewing()}
              />
            </div>
          </Show>
        </div>
        <Show when={blockedOpen()}>
          <div class="max-h-40 overflow-y-auto border-t border-border px-3 py-2">
            <For each={["notVanilla", "internalState", "resourcePacksOff", "unchecked"] as BlockReason[]}>
              {reason => {
                const rows = () => blocked().filter(row => row.reason === reason);
                return (
                  <Show when={rows().length > 0}>
                    <p class="mt-1 text-[11px] font-medium text-foreground">
                      {rows().length} — {REASON_LABELS[reason]}
                    </p>
                    <p class="text-[11px] text-muted-foreground break-words">
                      {rows().map(row => row.key).join(", ")}
                    </p>
                  </Show>
                );
              }}
            </For>
          </div>
        </Show>
      </div>

      <div class="flex flex-wrap items-center gap-2">
        <Show when={previewing()}>
          <button
            type="button"
            onClick={copyFromSource}
            class="rounded-md bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground hover:bg-primary/90"
          >
            Bring these settings into “{modlistName()}”
          </button>
          <span class="text-xs text-muted-foreground">
            viewing {sourceLabel()}, read-only: the other actions come back when you pick
            “default”
          </span>
        </Show>
        <Show when={!previewing()}>
          {/*
            Ri-applicare e promuovere sono gesti di una modlist: dal globale
            non c'è un'istanza a cui applicare, e promuovere il globale a sé
            stesso non vuol dire niente.
          */}
          <Show when={modlistName() !== null}>
            <Select
              value={instance()}
              onChange={setInstance}
              placeholder="pick an instance"
              align="right"
              panelClass="max-w-[18rem]"
              class="max-w-[14rem] rounded-md border border-input bg-input px-2 py-1 text-xs text-foreground"
              options={instances().map(name => ({ value: name, label: name }))}
              title="The instances of this mod-list"
            />
            <button
              type="button"
              onClick={reapply}
              disabled={!instance()}
              class="rounded-md bg-secondary px-3 py-1.5 text-xs text-secondary-foreground hover:bg-secondary/80 disabled:opacity-50"
            >
              Re-apply to instance
            </button>
            <button
              type="button"
              onClick={promoteToGlobal}
              class="ml-auto rounded-md bg-secondary px-3 py-1.5 text-xs text-secondary-foreground hover:bg-secondary/80"
            >
              Promote to global default
            </button>
          </Show>
          <button
            type="button"
            onClick={() => void save()}
            disabled={missingVersion()}
            title={missingVersion() ? "The file has no version: line" : undefined}
            // Un solo `ml-auto` per riga: con due, lo spazio libero si divide
            // fra i due bottoni e «Promote» resta a mezz'aria.
            class={`rounded-md bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground hover:bg-primary/90 disabled:opacity-50 ${
              modlistName() === null ? "ml-auto" : ""
            }`}
          >
            Save
          </button>
        </Show>
      </div>

      <Show when={confirm()}>
        <div
          class="fixed inset-0 z-[60] flex items-center justify-center bg-black/60 px-4"
          data-testid="options-confirm"
        >
          <div class="w-full max-w-md rounded-lg border border-border bg-card p-5 shadow-xl">
            <h3 class="text-sm font-semibold text-foreground">{confirm()!.title}</h3>
            <p class="mt-2 text-xs text-muted-foreground">{confirm()!.detail}</p>
            <div class="mt-4 flex justify-end gap-2">
              <button
                type="button"
                // Annullare chiude la conferma e basta: l'anteprima che si sta
                // guardando resta quella, altrimenti il pannello direbbe
                // "default" mostrando le righe di un'altra modlist.
                onClick={() => setConfirm(null)}
                class="rounded-md bg-secondary px-3 py-1.5 text-xs text-secondary-foreground hover:bg-secondary/80"
              >
                Cancel
              </button>
              <button
                type="button"
                onClick={() => {
                  const pending = confirm();
                  setConfirm(null);
                  if (pending) void pending.run();
                }}
                class="rounded-md bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground hover:bg-primary/90"
              >
                Overwrite
              </button>
            </div>
          </div>
        </div>
      </Show>
    </div>
  );
}
