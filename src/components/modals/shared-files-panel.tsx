import { invoke } from "@tauri-apps/api/core";
import { For, Show, createSignal, onMount } from "solid-js";
import { MaterialIcon } from "../icons";

/**
 * La condivisione dei tre file d'istanza (`servers.dat`, `hotbar.nbt`,
 * `command_history.txt`), dentro i Settings della modlist.
 *
 * Il pezzo che questa schermata deve rendere impossibile da fraintendere è
 * **chi adotta la lista di chi**: collegando un'altra modlist, è questa a
 * prendere i file di quella, non il contrario. Il verso non si può dedurre dal
 * nome del bottone, quindi è scritto due volte — nell'elenco e nella conferma —
 * con i due nomi veri dentro.
 */

export type Invoke = (command: string, args?: Record<string, unknown>) => Promise<unknown>;

export type SharedFileKey = "servers" | "hotbar" | "commandHistory";

export type SharedFileToggle = {
  file: string;
  key: SharedFileKey;
  filename: string;
  enabled: boolean;
};

export type SharedFilesView = {
  modlist: string;
  group: string[];
  canonical: string;
  linkable: string[];
  files: SharedFileToggle[];
};

/**
 * Perché un interruttore per file e non uno solo per tutti e tre: i rischi non
 * sono confrontabili, e la riga sotto ogni nome è il posto dove dirlo prima che
 * l'utente accenda.
 */
const FILE_COPY: Record<SharedFileKey, { title: string; detail: string }> = {
  servers: {
    title: "Multiplayer server list",
    detail: "The servers you added, with the icons and names the game downloaded.",
  },
  hotbar: {
    title: "Saved creative hotbars",
    detail:
      "Only travels to a game that is the same version or newer. An older game reads newer items as empty and would save the empty hotbars back.",
  },
  commandHistory: {
    title: "Command history",
    detail:
      "The last 50 commands you typed, exactly as typed. A server login such as /login <password> is one of them. Never included in an exported mod list.",
  },
};

interface Props {
  modlist: string;
  /** Iniettabile per poter mostrare il pannello fuori da Tauri. */
  invoke?: Invoke;
  /** Vista iniziale, quando il chiamante l'ha già caricata. */
  initialView?: SharedFilesView;
}

export function SharedFilesPanel(props: Props) {
  const call = (): Invoke => props.invoke ?? invoke;

  const [view, setView] = createSignal<SharedFilesView | null>(props.initialView ?? null);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [picking, setPicking] = createSignal(false);
  const [confirmLink, setConfirmLink] = createSignal<string | null>(null);
  const [confirmUnlink, setConfirmUnlink] = createSignal(false);

  const run = async (command: string, args: Record<string, unknown>) => {
    if (busy()) return;
    setBusy(true);
    setError(null);
    try {
      const next = (await call()(command, args)) as SharedFilesView;
      setView(next);
    } catch (reason) {
      setError(String(reason));
    } finally {
      setBusy(false);
    }
  };

  onMount(() => {
    if (props.initialView) return;
    void run("shared_files_view_command", { modlist: props.modlist });
  });

  const linked = () => (view()?.group.length ?? 0) > 1;
  /** Le altre del gruppo, cioè tutte tranne questa. */
  const partners = () => (view()?.group ?? []).filter(name => name !== props.modlist);

  return (
    <div class="flex flex-col gap-4" data-testid="shared-files-panel">
      <div class="rounded-md border border-border bg-background px-3 py-2 text-xs text-muted-foreground">
        These files live in each instance folder. The launcher copies them in before the game
        starts and back out after it exits, so every instance of this mod list sees the same
        ones.
      </div>

      <Show when={error()}>
        <div
          class="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive"
          data-testid="shared-files-error"
        >
          {error()}
        </div>
      </Show>

      <div class="flex flex-col gap-2">
        <For each={view()?.files ?? []}>
          {toggle => (
            <div
              class="flex items-start gap-3 rounded-lg border border-border bg-card px-4 py-3"
              data-testid={`shared-file-row-${toggle.key}`}
            >
              <div class="min-w-0 flex-1">
                <p class="text-sm font-medium">{FILE_COPY[toggle.key].title}</p>
                <p class="mt-0.5 text-xs text-muted-foreground">{FILE_COPY[toggle.key].detail}</p>
                <code class="mt-1 block text-[11px] text-muted-foreground/80">
                  {toggle.filename}
                </code>
              </div>
              <button
                role="switch"
                aria-checked={toggle.enabled}
                aria-label={`Share ${FILE_COPY[toggle.key].title}`}
                disabled={busy()}
                data-testid={`shared-file-switch-${toggle.key}`}
                onClick={() =>
                  void run("set_shared_file_enabled_command", {
                    modlist: props.modlist,
                    file: toggle.key,
                    enabled: !toggle.enabled,
                  })
                }
                class={`mt-1 h-6 w-11 shrink-0 rounded-full border transition-colors ${
                  toggle.enabled ? "border-primary bg-primary" : "border-border bg-muted"
                }`}
              >
                <span
                  class={`block h-4 w-4 rounded-full bg-background transition-transform ${
                    toggle.enabled ? "translate-x-6" : "translate-x-1"
                  }`}
                />
              </button>
            </div>
          )}
        </For>
      </div>

      <div class="rounded-lg border border-border bg-card px-4 py-3">
        <p class="text-sm font-medium">Linked mod lists</p>
        <Show
          when={linked()}
          fallback={
            <p class="mt-0.5 text-xs text-muted-foreground">
              Only the instances of <strong>{props.modlist}</strong> share these files. Link
              another mod list to share them across both.
            </p>
          }
        >
          <p class="mt-0.5 text-xs text-muted-foreground" data-testid="shared-files-group">
            <strong>{props.modlist}</strong> shares these files with{" "}
            <strong>{partners().join(", ")}</strong>. The copy everyone reads lives in{" "}
            <strong>{view()?.canonical ?? props.modlist}</strong>.
          </p>
        </Show>

        <div class="mt-3 flex flex-wrap gap-2">
          <Show when={!picking()}>
            <button
              class="rounded-md border border-border px-3 py-1.5 text-xs font-medium hover:bg-accent disabled:opacity-50"
              disabled={busy() || (view()?.linkable.length ?? 0) === 0}
              data-testid="shared-files-link-open"
              onClick={() => setPicking(true)}
            >
              Link another mod list…
            </button>
          </Show>
          <Show when={linked()}>
            <button
              class="rounded-md border border-border px-3 py-1.5 text-xs font-medium hover:bg-accent disabled:opacity-50"
              disabled={busy()}
              data-testid="shared-files-unlink-open"
              onClick={() => setConfirmUnlink(true)}
            >
              Unlink {props.modlist}
            </button>
          </Show>
        </div>

        <Show when={picking()}>
          <div class="mt-3 rounded-md border border-border bg-background p-3" data-testid="shared-files-picker">
            <p class="text-xs text-muted-foreground">
              Pick the mod list to share with. <strong>{props.modlist}</strong> will start using
              its files.
            </p>
            <div class="mt-2 flex flex-col gap-1">
              <For
                each={view()?.linkable ?? []}
                fallback={
                  <p class="text-xs text-muted-foreground">There is no other mod list yet.</p>
                }
              >
                {name => (
                  <button
                    class="flex items-center justify-between rounded-md border border-border px-3 py-2 text-left text-sm hover:bg-accent"
                    data-testid={`shared-files-link-candidate-${name}`}
                    onClick={() => setConfirmLink(name)}
                  >
                    <span>{name}</span>
                    <MaterialIcon name="link" size="sm" class="text-muted-foreground" />
                  </button>
                )}
              </For>
            </div>
            <button
              class="mt-2 text-xs text-muted-foreground hover:text-foreground"
              onClick={() => setPicking(false)}
            >
              Cancel
            </button>
          </div>
        </Show>
      </div>

      <Show when={confirmLink()}>
        {other => (
          <div
            class="rounded-lg border border-primary/40 bg-primary/5 px-4 py-3"
            data-testid="shared-files-link-confirm"
          >
            <p class="text-sm font-medium">
              Use the files of {other()} in {props.modlist}?
            </p>
            {/*
              La riga che porta tutto il peso: il verso. Chi conferma sta
              sostituendo la propria lista, non fondendo le due.
            */}
            <p class="mt-1 text-xs text-muted-foreground">
              <strong>{props.modlist}</strong> will adopt the server list, hotbars and command
              history of <strong>{other()}</strong>. The ones {props.modlist} has now are
              replaced, not merged. The replacement happens at the next launch of each
              instance, and the first time an instance's file is replaced the old one is kept
              next to it as <code>.bak</code>.
            </p>
            <div class="mt-3 flex gap-2">
              <button
                class="rounded-md bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground disabled:opacity-50"
                disabled={busy()}
                data-testid="shared-files-link-confirm-yes"
                onClick={() => {
                  const target = other();
                  setConfirmLink(null);
                  setPicking(false);
                  void run("link_shared_files_command", {
                    modlist: props.modlist,
                    other: target,
                  });
                }}
              >
                Use {other()}'s files
              </button>
              <button
                class="rounded-md border border-border px-3 py-1.5 text-xs font-medium hover:bg-accent"
                onClick={() => setConfirmLink(null)}
              >
                Cancel
              </button>
            </div>
          </div>
        )}
      </Show>

      <Show when={confirmUnlink()}>
        <div
          class="rounded-lg border border-border bg-background px-4 py-3"
          data-testid="shared-files-unlink-confirm"
        >
          <p class="text-sm font-medium">Stop sharing with {partners().join(", ")}?</p>
          <p class="mt-1 text-xs text-muted-foreground">
            Nobody loses anything. Both sides keep a copy of the files they are seeing right
            now — <strong>{props.modlist}</strong> on one side, {partners().join(", ")} on the
            other — and from then on the two go their own way.
          </p>
          <div class="mt-3 flex gap-2">
            <button
              class="rounded-md bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground disabled:opacity-50"
              disabled={busy()}
              data-testid="shared-files-unlink-confirm-yes"
              onClick={() => {
                setConfirmUnlink(false);
                void run("unlink_shared_files_command", { modlist: props.modlist });
              }}
            >
              Unlink
            </button>
            <button
              class="rounded-md border border-border px-3 py-1.5 text-xs font-medium hover:bg-accent"
              onClick={() => setConfirmUnlink(false)}
            >
              Cancel
            </button>
          </div>
        </div>
      </Show>
    </div>
  );
}
