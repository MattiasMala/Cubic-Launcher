// The global screenshot gallery.
//
// Not a tab of the editor: one grid over every instance of every mod list,
// newest first, split into periods (this month, then one group per month).
// Every thumbnail states which mod list and instance it came from, otherwise
// the grid mixes contexts without explaining itself.
//
// Order and grouping both read `modifiedMs`, which the backend takes from the
// filesystem rather than from the Minecraft-style filename
// (`screenshots.rs:list_screenshots`).

import { For, Show, createMemo, createResource, createSignal, onCleanup, onMount } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { modListCards, pushUiError } from "../store";
import type { ScreenshotEntry, ScreenshotListing } from "../lib/types";
import { MaterialIcon } from "./icons";

/** Thumbnails are read one by one, on mount, so the grid paints before the megabytes arrive. */
function Thumbnail(props: { entry: ScreenshotEntry }) {
  const [source] = createResource(
    () => props.entry.path,
    async (path) => await invoke<string>("read_image_as_data_url_command", { path }),
  );

  return (
    <Show
      when={source()}
      fallback={
        <div class="w-full h-full flex items-center justify-center bg-bgHover text-textMuted">
          <MaterialIcon name={source.error ? "broken_image" : "image"} size="lg" />
        </div>
      }
    >
      <img src={source()} alt={props.entry.fileName} class="w-full h-full object-cover" />
    </Show>
  );
}

export function ScreenshotsView() {
  const [listing, { refetch }] = createResource(
    async () => await invoke<ScreenshotListing>("list_screenshots_command"),
  );
  const [viewing, setViewing] = createSignal<ScreenshotEntry | null>(null);
  const [pendingDelete, setPendingDelete] = createSignal<ScreenshotEntry | null>(null);
  const [deleteBusy, setDeleteBusy] = createSignal(false);
  /**
   * Set when a trashing attempt came back refused: the dialog stays open and
   * switches to the permanent wording, carrying why the trash did not work.
   * Nothing has been deleted at that point — that is the whole pact.
   */
  const [trashRefusal, setTrashRefusal] = createSignal<string | null>(null);

  const trashSupported = () => listing()?.trashSupported ?? false;

  /**
   * Consecutive runs of the (already newest-first) listing: the current
   * calendar month, then one group per month.
   */
  const periods = createMemo(() => {
    const now = new Date();
    const groups: Array<{ key: string; label: string; entries: ScreenshotEntry[] }> = [];

    for (const entry of listing()?.entries ?? []) {
      const taken = new Date(entry.modifiedMs);
      const thisMonth =
        taken.getFullYear() === now.getFullYear() && taken.getMonth() === now.getMonth();
      const key = thisMonth ? "this-month" : `${taken.getFullYear()}-${taken.getMonth()}`;
      const last = groups[groups.length - 1];
      if (last && last.key === key) {
        last.entries.push(entry);
        continue;
      }
      groups.push({
        key,
        label: thisMonth
          ? "This month"
          : taken.toLocaleDateString(undefined, { month: "long", year: "numeric" }),
        entries: [entry],
      });
    }

    return groups;
  });

  const modlistLabel = (entry: ScreenshotEntry) => {
    const card = modListCards().find((modList) => modList.name === entry.modlistName);
    return card?.displayName || entry.modlistName;
  };

  const openFolder = async (entry: ScreenshotEntry) => {
    try {
      // The backend takes the three names, never a path: it rebuilds the
      // location itself, so nothing the frontend sends can point elsewhere.
      await invoke("open_screenshot_folder_command", {
        modlistName: entry.modlistName,
        instanceName: entry.instanceName,
        fileName: entry.fileName,
      });
    } catch (error) {
      pushUiError({
        title: "Could not open the folder",
        message: `The folder of '${entry.fileName}' could not be opened.`,
        detail: String(error),
        severity: "error",
        scope: "launch",
      });
    }
  };

  const confirmDelete = async () => {
    const entry = pendingDelete();
    if (!entry) return;

    setDeleteBusy(true);
    try {
      // `allowPermanent` is exactly what the dialog promised. On a platform
      // with a trash the first attempt promises the trash; if the backend
      // comes back with `trash-unavailable:` nothing was deleted, and the
      // dialog asks again saying "permanently" before we set the flag.
      const permanentPromised = !trashSupported() || trashRefusal() !== null;
      await invoke("delete_screenshot_command", {
        modlistName: entry.modlistName,
        instanceName: entry.instanceName,
        fileName: entry.fileName,
        allowPermanent: permanentPromised,
      });
      setPendingDelete(null);
      setTrashRefusal(null);
      if (viewing()?.path === entry.path) setViewing(null);
      await refetch();
    } catch (error) {
      const reported = String(error);
      if (reported.includes("trash-unavailable:")) {
        setTrashRefusal(reported.split("trash-unavailable:").pop()?.trim() || reported);
      } else {
        pushUiError({
          title: "Could not delete the screenshot",
          message: `'${entry.fileName}' is still on disk.`,
          detail: reported,
          severity: "error",
          scope: "launch",
        });
      }
    } finally {
      setDeleteBusy(false);
    }
  };

  onMount(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      if (pendingDelete()) {
        setPendingDelete(null);
        setTrashRefusal(null);
        return;
      }
      setViewing(null);
    };
    window.addEventListener("keydown", onKeyDown);
    onCleanup(() => window.removeEventListener("keydown", onKeyDown));
  });

  return (
    <div class="flex-1 min-h-0 overflow-y-auto bg-bgDark">
      <div class="px-6 py-5 flex items-baseline gap-3">
        <h1 class="text-xl font-semibold text-white">Screenshots</h1>
        <span class="text-sm text-textMuted">
          {listing()?.entries.length ?? 0} from every instance
        </span>
      </div>

      <Show when={listing.error}>
        <p class="px-6 pb-6 text-sm text-destructive">
          The screenshots could not be read: {String(listing.error)}
        </p>
      </Show>

      <Show when={!listing.loading && (listing()?.entries.length ?? 0) === 0 && !listing.error}>
        <p class="px-6 pb-6 text-sm text-textMuted">
          No screenshots yet. Minecraft writes them with F2, and they show up here.
        </p>
      </Show>

      <For each={periods()}>
        {(period) => (
          <section class="px-6 pb-8">
            <h2 class="text-xs uppercase font-semibold tracking-wider text-textMuted mb-3">
              {period.label}
            </h2>
            <div class="grid grid-cols-1 sm:grid-cols-2 xl:grid-cols-3 2xl:grid-cols-4 gap-4">
              <For each={period.entries}>
                {(entry) => (
                  <figure class="group rounded-lg border border-borderColor bg-bgPanel overflow-hidden">
                    <button
                      type="button"
                      onClick={() => setViewing(entry)}
                      class="relative block w-full aspect-video bg-bgHover cursor-pointer"
                    >
                      <Thumbnail entry={entry} />
                      <span class="absolute inset-0 bg-black/0 group-hover:bg-black/20 transition-colors duration-75" />
                    </button>
                    <figcaption class="p-3 flex items-start gap-2">
                      <div class="min-w-0 flex-1">
                        <p class="text-sm text-white truncate" title={`${modlistLabel(entry)} / ${entry.instanceName}`}>
                          {modlistLabel(entry)}
                          <span class="text-textMuted"> / {entry.instanceName}</span>
                        </p>
                        <p class="text-xs text-textMuted truncate">
                          {new Date(entry.modifiedMs).toLocaleString(undefined, {
                            day: "2-digit",
                            month: "short",
                            year: "numeric",
                            hour: "2-digit",
                            minute: "2-digit",
                          })}
                          {" · "}
                          {(entry.sizeBytes / 1_000_000).toFixed(1)} MB
                        </p>
                      </div>
                      <div class="flex items-center gap-1 shrink-0">
                        <button
                          type="button"
                          onClick={() => void openFolder(entry)}
                          class="w-8 h-8 rounded-md text-textMuted hover:text-white hover:bg-bgHover flex items-center justify-center cursor-pointer"
                          aria-label={`Open the folder of ${entry.fileName}`}
                        >
                          <MaterialIcon name="folder_open" size="sm" />
                        </button>
                        <button
                          type="button"
                          onClick={() => {
                            // A refusal belongs to the deletion that hit it:
                            // left set, the next dialog would open straight
                            // in permanent mode and never retry the trash.
                            setTrashRefusal(null);
                            setPendingDelete(entry);
                          }}
                          class="w-8 h-8 rounded-md text-textMuted hover:text-destructive hover:bg-bgHover flex items-center justify-center cursor-pointer"
                          aria-label={`Delete ${entry.fileName}`}
                        >
                          <MaterialIcon name="delete" size="sm" />
                        </button>
                      </div>
                    </figcaption>
                  </figure>
                )}
              </For>
            </div>
          </section>
        )}
      </For>

      <Show when={viewing()}>
        {entry => (
          <div
            class="fixed inset-0 z-[80] bg-black/85 flex flex-col items-center justify-center p-8"
            onClick={() => setViewing(null)}
          >
            <div class="max-h-full max-w-full flex flex-col items-center gap-3" onClick={event => event.stopPropagation()}>
              <div class="max-h-[80vh] max-w-[90vw] overflow-hidden rounded-lg border border-borderColor">
                <LargePreview entry={entry()} />
              </div>
              <p class="text-sm text-textMuted">
                {entry().fileName} — {modlistLabel(entry())} / {entry().instanceName}
              </p>
            </div>
          </div>
        )}
      </Show>

      <Show when={pendingDelete()}>
        {entry => (
          <div class="fixed inset-0 z-[90] bg-black/70 flex items-center justify-center p-6">
            <div class="w-[420px] max-w-full rounded-lg border border-borderColor bg-bgPanel p-5">
              <h2 class="text-base font-semibold text-white mb-2">
                {trashSupported() && !trashRefusal()
                  ? "Move this screenshot to the trash?"
                  : "Delete this screenshot?"}
              </h2>
              <p class="text-sm text-textMuted mb-1 break-all">{entry().fileName}</p>
              <p class="text-xs text-textMuted mb-4">
                {modlistLabel(entry())} / {entry().instanceName}
              </p>
              <p
                class={`text-sm mb-5 ${
                  trashSupported() && !trashRefusal() ? "text-textMuted" : "text-destructive"
                }`}
              >
                {trashRefusal()
                  ? `The system trash could not take it (${trashRefusal()}). Nothing has been deleted yet: going on deletes the file for good.`
                  : trashSupported()
                    ? "It goes to the system trash, where your file manager can put it back."
                    : "The system trash is not available here, so this deletes the file for good."}
              </p>
              <div class="flex justify-end gap-2">
                <button
                  type="button"
                  onClick={() => {
                    setPendingDelete(null);
                    setTrashRefusal(null);
                  }}
                  class="px-3 py-1.5 rounded-md text-sm text-textMuted hover:text-white hover:bg-bgHover cursor-pointer"
                >
                  Cancel
                </button>
                <button
                  type="button"
                  disabled={deleteBusy()}
                  onClick={() => void confirmDelete()}
                  class="px-3 py-1.5 rounded-md text-sm bg-destructive text-white hover:opacity-90 disabled:opacity-60 cursor-pointer"
                >
                  {trashSupported() && !trashRefusal() ? "Move to trash" : "Delete permanently"}
                </button>
              </div>
            </div>
          </div>
        )}
      </Show>
    </div>
  );
}

/** The full-size read, separate from the thumbnail so closing frees it. */
function LargePreview(props: { entry: ScreenshotEntry }) {
  const [source] = createResource(
    () => props.entry.path,
    async (path) => await invoke<string>("read_image_as_data_url_command", { path }),
  );

  return (
    <Show
      when={source()}
      fallback={<div class="w-[60vw] h-[40vh] flex items-center justify-center text-textMuted">Loading…</div>}
    >
      <img src={source()} alt={props.entry.fileName} class="block max-h-[80vh] max-w-[90vw] object-contain" />
    </Show>
  );
}
