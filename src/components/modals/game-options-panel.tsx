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
  keybind: "Comandi",
  soundCategory: "Volumi",
  modelPart: "Aspetto del personaggio",
};

/**
 * Il gruppo di ripiego. Su 1.20.1 è l'unico che esiste per le impostazioni
 * semplici — il jar è offuscato e i nomi delle schermate non sopravvivono —
 * quindi deve reggere da solo 86 righe: per questo la casella di ricerca sopra
 * l'elenco filtra su tutti i gruppi e questo resta ordinato per nome.
 */
const FALLBACK_GROUP = "Altre impostazioni";

const REASON_LABELS: Record<BlockReason, string> = {
  notVanilla: "non è una chiave vanilla (quasi sempre di un mod)",
  internalState: "è stato di quella installazione, non una preferenza",
  resourcePacksOff: "la lista dei resource pack, spenta per questo salto",
  unchecked: "spunta tolta",
};


interface Props {
  modlist: string;
  /** Le altre modlist, per il dropdown che copia le loro impostazioni. */
  otherModlists: string[];
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
  const [jump, setJump] = createSignal<"instance" | "modlist">("instance");
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
  const scope = (): OptionsScope => ({ level: "modlist", modlist: props.modlist });

  /**
   * Quale file si sta guardando. `default` è la modlist corrente; le altre
   * voci — le altre modlist e le istanze di questa — **non copiano niente**,
   * aprono quel file in lettura. Così le spunte e il contatore descrivono la
   * sorgente vera di quel salto, che è quello che chiede D67, e per portarsela
   * qui serve il bottone apposta, che chiede conferma.
   */
  const previewing = () => source() !== "default";
  const sourceScope = (): OptionsScope => {
    const selected = source();
    if (selected.startsWith("i:")) {
      return { level: "instance", modlist: props.modlist, instance: selected.slice(2) };
    }
    if (selected.startsWith("m:")) return { level: "modlist", modlist: selected.slice(2) };
    return scope();
  };
  const sourceLabel = () => {
    const selected = source();
    if (selected.startsWith("i:")) return `istanza ${selected.slice(2)}`;
    if (selected.startsWith("m:")) return `modlist ${selected.slice(2)}`;
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
      const listed = (await call()("list_instance_files_command", {
        modlistName: props.modlist,
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
      return `${status.report.seeded} impostazioni scritte in ${status.report.target}; ${status.report.blocked.length} non passate.`;
    }
    if (status.status === "refused") return `Rifiutato: ${status.reason}`;
    if (status.status === "skippedNoSource") return `Niente da copiare: ${status.source} non esiste.`;
    return `Niente da fare: ${status.target} esiste già.`;
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
      await call()("save_shared_options_command", { scope: scope(), entries });
      setNotice("Salvato.");
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
    // In anteprima l'unico gesto è "porta qui", cioè un salto verso questa
    // modlist; tornando a «default» il gesto disponibile è di nuovo la
    // ri-applicazione a un'istanza. Il selettore del contatore segue, se no
    // le spunte mostrano un default e il bottone ne usa un altro.
    setJump(value === "default" ? "instance" : "modlist");
    setNotice(
      value === "default"
        ? null
        : `Stai guardando ${sourceLabel()}. Non è stato copiato niente.`,
    );
    void load();
  };

  const copyFromSource = () => {
    if (!previewing()) return;
    setConfirm({
      title: `Portare qui le impostazioni di ${sourceLabel()}?`,
      detail: `Sovrascrive mod-lists/${props.modlist}/options.txt, che è il default da cui nascono le istanze nuove. Le istanze già avviate non cambiano.`,
      run: async () => {
        await apply(sourceScope(), scope(), false);
        setSource("default");
        setCheckedOverride({});
        await load();
      },
    });
  };

  const reapply = () => {
    const target = instance();
    if (!target) return;
    setConfirm({
      title: `Ri-applicare a «${target}»?`,
      detail: `Sovrascrive mod-lists/${props.modlist}/instances/${target}/options.txt, che da quando esiste è del gioco: quello che hai cambiato in gioco in quell'istanza va perso.`,
      run: () => apply(scope(), { level: "instance", modlist: props.modlist, instance: target }, true),
    });
  };

  const promoteToGlobal = () => {
    setConfirm({
      title: "Promuovere a default globale?",
      detail: "Sovrascrive options.txt globale, che è il modello per le modlist nuove. Le modlist che esistono già non cambiano.",
      run: () => apply(scope(), { level: "global" }, false),
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
        <div class="ml-auto flex items-center gap-2">
          <span class="text-muted-foreground">Da</span>
          <Select
            value={source()}
            onChange={onSourceChange}
            class="rounded-md border border-input bg-input px-2 py-1 text-xs text-foreground"
            options={[
              { value: "default", label: "default" },
              ...props.otherModlists.map(name => ({ value: `m:${name}`, label: `modlist ${name}` })),
              ...instances().map(name => ({ value: `i:${name}`, label: `istanza ${name}` })),
            ]}
            title="Guarda le impostazioni di un'altra modlist o di un'istanza, senza copiarle"
          />
        </div>
      </div>

      <Show when={view() && !view()!.exists}>
        <p class="rounded-md border border-border bg-muted/30 px-3 py-2 text-xs text-muted-foreground">
          Questa modlist non ha ancora un <code>options.txt</code>. Nasce dal globale alla
          creazione, oppure si promuove da un'istanza qui sotto.
        </p>
      </Show>

      <Show when={missingVersion()}>
        <p
          class="rounded-md border border-red-700/40 bg-red-900/20 px-3 py-2 text-xs text-red-300"
          data-testid="missing-version"
        >
          A questo file manca la riga <code>version:</code>, e senza quella il gioco lo
          tratta come DataVersion 0: alla prima lettura gli applicherebbe tutti i
          datafixer delle opzioni, compreso quello che rimappa i codici dei tasti. Non ne
          invento una, e non prenderla da un'istanza a caso: quel numero dice chi ha
          scritto <em>questi</em> valori, e appiccicarne un altro li fa migrare storti.
          O rimetti la riga che c'era, o scegli un'istanza nel menu «Da» qui sopra e premi
          «Porta queste impostazioni», che porta file e numero insieme. Fino ad allora
          salvare è disattivato.
        </p>
      </Show>

      <Show when={view()?.derivationError && !missingVersion()}>
        <p class="rounded-md border border-amber-700/40 bg-amber-900/20 px-3 py-2 text-xs text-amber-300">
          Non so dire quali chiavi siano vanilla: {view()!.derivationError}. Finché è così, non
          si semina niente.
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
          placeholder="Cerca fra le impostazioni…"
          class="w-full rounded-md border border-input bg-input px-3 py-1.5 text-sm text-foreground focus:outline-none focus:ring-1 focus:ring-ring"
        />
      </div>

      <div class="max-h-[46vh] overflow-y-auto rounded-md border border-border">
        <Show
          when={!loading()}
          fallback={<p class="px-3 py-6 text-center text-xs text-muted-foreground">Carico…</p>}
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
                              ? "Copia questa riga quando si semina"
                              : "Non è vanilla: non si semina mai"
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
                              ? "Stai guardando un altro file: qui non si modifica"
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
            {blocked().length} righe non passano
          </button>
          <div class="ml-auto flex items-center gap-2">
            <span class="text-muted-foreground">seminando verso</span>
            <Select
              value={jump()}
              onChange={value => setJump(value as "instance" | "modlist")}
              class="rounded-md border border-input bg-input px-2 py-0.5 text-xs text-foreground"
              options={[
                { value: "instance", label: "un'istanza di questa modlist" },
                { value: "modlist", label: "un'altra modlist o il globale" },
              ]}
              title="I default cambiano con il salto (D67)"
              disabled={previewing()}
            />
          </div>
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
            Porta queste impostazioni in «{props.modlist}»
          </button>
          <span class="text-xs text-muted-foreground">
            stai guardando {sourceLabel()}: gli altri gesti tornano scegliendo «default»
          </span>
        </Show>
        <Show when={!previewing()}>
          <Select
            value={instance()}
            onChange={setInstance}
            placeholder="scegli un'istanza"
            class="rounded-md border border-input bg-input px-2 py-1 text-xs text-foreground"
            options={instances().map(name => ({ value: name, label: name }))}
            title="Le istanze di questa modlist"
          />
          <button
            type="button"
            onClick={reapply}
            disabled={!instance()}
            class="rounded-md bg-secondary px-3 py-1.5 text-xs text-secondary-foreground hover:bg-secondary/80 disabled:opacity-50"
          >
            Ri-applica all'istanza
          </button>
          <button
            type="button"
            onClick={promoteToGlobal}
            class="ml-auto rounded-md bg-secondary px-3 py-1.5 text-xs text-secondary-foreground hover:bg-secondary/80"
          >
            Promuovi a default globale
          </button>
          <button
            type="button"
            onClick={() => void save()}
            disabled={missingVersion()}
            title={missingVersion() ? "Manca la riga version: nel file" : undefined}
            class="rounded-md bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground hover:bg-primary/90 disabled:opacity-50"
          >
            Salva
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
                Annulla
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
                Sovrascrivi
              </button>
            </div>
          </div>
        </div>
      </Show>
    </div>
  );
}
