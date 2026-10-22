import { Show, createSignal, onCleanup, onMount } from "solid-js";
import { UploadIcon } from "../icons";
import type { LocalImportFailure } from "../../lib/contentImportErrors";
import type { LocalPickMode, LocalUploadContentType } from "./shared";

export function LocalJarTab(props: {
  contentType: LocalUploadContentType;
  onUploadLocal: (contentType: LocalUploadContentType, pick: LocalPickMode) => Promise<LocalImportFailure | null>;
  onDropLocal?: (path: string, contentType: LocalUploadContentType) => Promise<LocalImportFailure | null>;
}) {
  const [dragging, setDragging] = createSignal(false);
  const [busy, setBusy] = createSignal(false);
  // A refused import is shown here and not in the notice banner: the banner
  // sits under the header, and this dialog covers it.
  const [failure, setFailure] = createSignal<LocalImportFailure | null>(null);

  const isMod = () => props.contentType === "mod";
  const fileExt = () => isMod() ? ".jar" : ".zip";
  const fileLabel = () => isMod() ? "JAR" : "ZIP";
  const typeLabel = () => {
    switch (props.contentType) {
      case "resourcepack": return "Resource Pack";
      case "datapack": return "Data Pack";
      case "shader": return "Shader";
      default: return "JAR";
    }
  };

  const run = async (action: () => Promise<LocalImportFailure | null>) => {
    if (busy()) return;
    setBusy(true);
    setFailure(null);
    try {
      setFailure(await action());
    } finally {
      setBusy(false);
    }
  };

  // Il cleanup si registra subito, mentre Solid ha ancora un owner: dopo un
  // `await` `onCleanup` non registra niente, e il listener restava vivo a
  // dialog chiuso, aggiungendo in silenzio ogni .jar trascinato sulla finestra
  // (audit 101, ui-schermate-6). Se la tab si chiude prima che la
  // registrazione finisca, il listener appena nato si toglie da solo.
  let disposed = false;
  let unlisten: (() => void) | undefined;
  onCleanup(() => {
    disposed = true;
    unlisten?.();
    unlisten = undefined;
  });

  onMount(() => {
    void (async () => {
      try {
        const { getCurrentWindow } = await import("@tauri-apps/api/window");
        if (disposed) return;
        const win = getCurrentWindow();
        const stop = await win.onDragDropEvent(async event => {
          if (event.payload.type === "over" || event.payload.type === "enter") {
            setDragging(true);
          } else if (event.payload.type === "leave") {
            setDragging(false);
          } else if (event.payload.type === "drop") {
            setDragging(false);
            const paths: string[] = event.payload.paths ?? [];
            // A mod is a .jar and nothing else. A pack is a .zip *or* a folder,
            // and a dropped folder arrives as a plain path with no extension to
            // match on, so anything that is not a .zip is handed to the backend,
            // which is the only side that can tell a folder from a stray file.
            const matchedPath = isMod()
              ? paths.find(path => path.toLowerCase().endsWith(fileExt()))
              : paths.find(path => path.toLowerCase().endsWith(".zip")) ?? paths[0];
            if (matchedPath && props.onDropLocal) {
              await run(() => props.onDropLocal!(matchedPath, props.contentType));
            }
          }
        });
        if (disposed) stop();
        else unlisten = stop;
      } catch {
        // not in Tauri
      }
    })();
  });

  return (
    <div class="space-y-4">
      <div
        class={`flex flex-col items-center justify-center rounded-lg border-2 border-dashed py-14 transition-colors ${
          dragging() ? "border-primary bg-primary/10" : "border-border bg-muted/20"
        }`}
      >
        <UploadIcon class={`mb-4 h-12 w-12 transition-colors ${dragging() ? "text-primary" : "text-muted-foreground/50"}`} />
        <h4 class="mb-1 font-medium text-foreground">
          {dragging() ? `Drop ${fileLabel()} file here` : `Upload ${typeLabel()}`}
        </h4>
        <p class="mb-4 max-w-xs text-center text-sm text-muted-foreground">
          <Show
            when={isMod()}
            fallback={<>Drag & drop a <code>.zip</code> or an unpacked pack folder here, or browse below.</>}
          >
            Drag & drop a <code>{fileExt()}</code> file here, or click Browse Files below.
          </Show>
        </p>
        <div class="flex w-full max-w-xs flex-col gap-3">
          <button
            disabled={busy()}
            onClick={() => void run(() => props.onUploadLocal(props.contentType, "file"))}
            class="rounded-md bg-secondary px-4 py-2 text-sm font-medium text-secondary-foreground transition-colors hover:bg-secondary/80 disabled:opacity-60"
          >
            {isMod() ? "Browse Files" : "Browse ZIP File"}
          </button>
          {/* A folder picker is a second button, not a second mode of the
              first: a Tauri dialog opens files or directories, never both, and
              a pack that is already unpacked has to be reachable. */}
          <Show when={!isMod()}>
            <button
              disabled={busy()}
              onClick={() => void run(() => props.onUploadLocal(props.contentType, "directory"))}
              class="rounded-md bg-secondary px-4 py-2 text-sm font-medium text-secondary-foreground transition-colors hover:bg-secondary/80 disabled:opacity-60"
            >
              Browse Pack Folder
            </button>
          </Show>
        </div>
        <Show when={isMod()}>
          <p class="mt-4 max-w-xs text-center text-xs text-warning">
            Local mods carry a dependency warning - you must manually verify and add required library mods.
          </p>
        </Show>
      </div>
      <Show when={failure()} keyed>
        {refused => (
          <div class="rounded-md border border-destructive/40 bg-destructive/10 px-4 py-3" role="alert">
            <p class="text-sm font-semibold text-foreground">{refused.title}</p>
            <p class="mt-1 text-xs text-muted-foreground">{refused.message}</p>
            <Show when={refused.detail}>
              <p class="mt-1 text-xs text-muted-foreground/80">{refused.detail}</p>
            </Show>
          </div>
        )}
      </Show>
    </div>
  );
}
