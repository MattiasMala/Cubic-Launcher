import { Show } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { ask } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  launchState,
  pushUiError,
  setUpdateBusy,
  setUpdateInfo,
  updateBusy,
  updateInfo,
} from "../store";
import { XIcon } from "./icons";

const RELEASES_URL = "https://github.com/MattiasMala/Cubic-Launcher/releases";
const isTauri = () => "__TAURI_INTERNALS__" in window;

const summarizeNotes = (notes: string | null) => {
  const compact = notes?.replace(/\s+/g, " ").trim();
  if (!compact) return null;
  return compact.length > 200 ? `${compact.slice(0, 197).trimEnd()}...` : compact;
};

/** The backend's prefix when it refuses to install while the game runs (`updater.rs`). */
const MINECRAFT_RUNNING = "minecraft-running:";

/**
 * Installing restarts the launcher, and with the game open the restart skips
 * the copy back of the shared files (D152). Asked here before the install; the
 * backend refuses without it, so a launcher that lost track of the game (a
 * reloaded window) still asks.
 */
const confirmInstallWhileRunning = async () => {
  return ask(
    "Minecraft is running or starting. Installing the update restarts Cubic Launcher: a launch still being prepared stops, and the server list, saved hotbars and command history the game changes reach the mod list only at the next launch of that instance.",
    {
      title: "Install the update now?",
      kind: "warning",
      okLabel: "Install and restart",
      cancelLabel: "Later",
    },
  );
};

export function UpdateBanner() {
  const handleInstallUpdate = async () => {
    if (!isTauri() || updateBusy()) return;

    let confirmedWhileRunning = false;
    // A launch still preparing counts too: the restart would cut it off
    // (review-frontend-shell-23), and the backend refuses it the same way.
    if (launchState() === "running" || launchState() === "resolving") {
      if (!(await confirmInstallWhileRunning())) return;
      confirmedWhileRunning = true;
    }

    setUpdateBusy(true);
    try {
      try {
        await invoke<void>("install_update", { confirmedWhileRunning });
      } catch (err) {
        if (confirmedWhileRunning || !String(err).includes(MINECRAFT_RUNNING)) throw err;
        if (!(await confirmInstallWhileRunning())) {
          setUpdateBusy(false);
          return;
        }
        await invoke<void>("install_update", { confirmedWhileRunning: true });
      }
    } catch (err) {
      pushUiError({
        title: "Update failed",
        message: String(err),
        detail: "Cubic Launcher could not install the update.",
        severity: "error",
        scope: "launch",
      });
      setUpdateBusy(false);
    }
  };

  const handleOpenReleaseNotes = async () => {
    if (!isTauri()) return;

    try {
      await openUrl(RELEASES_URL);
    } catch (err) {
      pushUiError({
        title: "Could not open release notes",
        message: "The release page could not be opened.",
        detail: String(err),
        severity: "error",
        scope: "launch",
      });
    }
  };

  return (
    <Show when={updateInfo()}>
      {info => (
        <aside class="shrink-0 border-b border-borderColor bg-bgPanel px-4 py-3" role="status">
          <div class="flex items-center gap-4">
            <div class="min-w-0 flex-1">
              <p class="text-sm font-semibold text-textMain">
                Cubic Launcher {info().version} available
              </p>
              <Show when={summarizeNotes(info().notes)}>
                {notes => <p class="mt-0.5 line-clamp-2 text-xs text-textMuted">{notes()}</p>}
              </Show>
            </div>
            <div class="flex shrink-0 items-center gap-3">
              <a
                href={RELEASES_URL}
                target="_blank"
                rel="noreferrer"
                onClick={event => {
                  if (!isTauri()) return;
                  event.preventDefault();
                  void handleOpenReleaseNotes();
                }}
                class="text-sm text-textMuted underline underline-offset-2 transition-colors hover:text-white"
              >
                Release notes
              </a>
              <button
                type="button"
                onClick={() => void handleInstallUpdate()}
                disabled={updateBusy()}
                class="rounded-md bg-primary px-3 py-1.5 text-sm font-medium text-white transition-colors hover:bg-primary/90 disabled:cursor-not-allowed disabled:opacity-50"
              >
                {updateBusy() ? "Updating..." : "Update now"}
              </button>
              <button
                type="button"
                onClick={() => setUpdateInfo(null)}
                class="flex h-8 w-8 items-center justify-center rounded-md text-textMuted transition-colors hover:bg-white/10 hover:text-white"
                aria-label="Dismiss update notification"
                title="Dismiss"
              >
                <XIcon class="h-4 w-4" />
              </button>
            </div>
          </div>
        </aside>
      )}
    </Show>
  );
}
