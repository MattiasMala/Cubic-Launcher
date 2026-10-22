import { createSignal, onCleanup, onMount } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { ask } from "@tauri-apps/plugin-dialog";
import { pushUiError, setSettingsModalOpen } from "../store";
import { MaterialIcon } from "./icons";
// 64×64 copy of assets/icon.png (1254×1254, 1.48 MB): the header draws it at
// 32 px, 64 for HiDPI. The original stays for the README (audit 101, 069 M5).
import logoUrl from "../../assets/icon-64.png";

const isTauri = () => "__TAURI_INTERNALS__" in window;

/**
 * Closing the window while Minecraft runs skips the copy back of the shared
 * files (servers.dat, hotbar.nbt, command_history.txt): it happens when the
 * game exits, and a closed launcher is not there to see it. The backend stops
 * every close request while the game runs — the button below, Alt+F4, the
 * taskbar — and asks here (D152).
 */
const CLOSE_WHILE_RUNNING_MESSAGE =
  "Minecraft is still running. If you close Cubic Launcher now, the server list, saved hotbars and command history the game changes reach the mod list only at the next launch of that instance.";

export function Header() {
  const [maximized, setMaximized] = createSignal(false);

  onMount(() => {
    if (!isTauri()) return;
    let disposed = false;
    let unlisten: (() => void) | null = null;
    void (async () => {
      const stop = await listen("close-requested-while-running", async () => {
        try {
          const close = await ask(CLOSE_WHILE_RUNNING_MESSAGE, {
            title: "Close Cubic Launcher?",
            kind: "warning",
            okLabel: "Close anyway",
            cancelLabel: "Keep open",
          });
          if (close) await invoke("close_launcher_anyway_command");
        } catch (err) {
          pushUiError({ title: "Window control failed", message: "The launcher could not be closed.", detail: String(err), severity: "error", scope: "launch" });
        }
      });
      if (disposed) stop();
      else unlisten = stop;
    })();
    onCleanup(() => {
      disposed = true;
      unlisten?.();
    });
  });

  const handleWindowAction = async (action: "minimize" | "toggle-maximize" | "close") => {
    if (!isTauri()) return;

    try {
      const { getCurrentWindow } = await import("@tauri-apps/api/window");
      const currentWindow = getCurrentWindow();

      if (action === "minimize") {
        await currentWindow.minimize();
        return;
      }

      if (action === "toggle-maximize") {
        if (await currentWindow.isMaximized()) {
          await currentWindow.unmaximize();
          setMaximized(false);
        } else {
          await currentWindow.maximize();
          setMaximized(true);
        }
        return;
      }

      await currentWindow.close();
    } catch (err) {
      pushUiError({ title: "Window control failed", message: "The desktop window action could not be completed.", detail: String(err), severity: "error", scope: "launch" });
    }
  };

  return (
    <header data-tauri-drag-region class="h-14 border-b border-borderColor bg-bgPanel flex items-center justify-between px-4 shrink-0 z-10 w-full">
      <div class="flex items-center gap-3">
        <img src={logoUrl} alt="Cubic Launcher" class="w-8 h-8 select-none" draggable={false} />
        <h1 class="font-semibold text-lg tracking-wide text-white/90">Cubic Launcher</h1>
      </div>
      <div class="flex items-center gap-4 text-textMuted">
        <button
          onClick={() => setSettingsModalOpen(true)}
          class="hover:text-white transition-colors duration-75"
          title="Settings"
        >
          <MaterialIcon name="settings" size="md" />
        </button>
        <button onClick={() => void handleWindowAction("minimize")} class="hover:text-white transition-colors duration-75" title="Minimize">
          <MaterialIcon name="remove" size="md" />
        </button>
        <button onClick={() => void handleWindowAction("toggle-maximize")} class="hover:text-white transition-colors duration-75" title={maximized() ? "Restore" : "Maximize"}>
          <MaterialIcon name={maximized() ? "filter_none" : "check_box_outline_blank"} size="md" />
        </button>
        <button onClick={() => void handleWindowAction("close")} class="hover:text-red-500 transition-colors duration-75" title="Close">
          <MaterialIcon name="close" size="md" />
        </button>
      </div>
    </header>
  );
}
