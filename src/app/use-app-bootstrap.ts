import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { onCleanup, onMount } from "solid-js";
import { logger } from "../lib/logger";
import {
  appendLaunchLog,
  launchState,
  launchStopRequested,
  modRowsState,
  pushUiError,
  selectedMcVersion,
  selectedModLoader,
  setActiveRailView,
  setLaunchStopRequested,
  setLaunchProgress,
  setLaunchStageDetail,
  setLaunchStageLabel,
  setLaunchState,
  setLogViewerOpen,
  setProfileRecordingPath,
} from "../store";
import {
  fetchModMetadata,
  isTauri,
  loadEditorSnapshot,
  loadModlistGroups,
  loadMinecraftVersions,
  loadModlistPresentation,
  loadShellSnapshot,
  runResolution,
} from "./backend-loaders";

interface AppBootstrapOptions {
  primePersistenceState: () => void;
}

export function useAppBootstrap(options: AppBootstrapOptions) {
  onMount(() => {
    let disposed = false;
    const unlisteners: Array<() => void> = [];

    // The listeners come first: a launch started as soon as the window shows
    // (a world's Play on the Home) must find its progress, logs and errors
    // listened to (stato-frontend-7).
    const registerListeners = () => Promise.all([
      listen<{ state: string; progress: number; stage: string; detail: string }>("launch-progress", (event) => {
        const { state, progress, stage, detail } = event.payload;
        setLaunchState(state as "idle" | "resolving" | "ready" | "running");
        setLaunchProgress(progress);
        setLaunchStageLabel(stage);
        setLaunchStageDetail(detail);
        // Un lancio nuovo: il profilo del precedente non è più quello da mostrare.
        if (state !== "idle") setProfileRecordingPath(null);
      }),

      listen<{ stream: string; line: string }>("minecraft-log", (event) => {
        appendLaunchLog(event.payload.line);
      }),

      listen<{ title: string; message: string; detail: string; severity?: "warning" | "error"; scope?: "launch" | "download" | "account" }>("launcher-error", (event) => {
        pushUiError({
          title: event.payload.title,
          message: event.payload.message,
          detail: event.payload.detail,
          severity: event.payload.severity ?? "error",
          scope: event.payload.scope ?? "launch",
        });
      }),

      listen<{ success: boolean; exitCode: number | null }>("minecraft-exit", (event) => {
        const { success, exitCode } = event.payload;
        const stoppedByUser = launchStopRequested();
        setLaunchStopRequested(false);
        setLaunchState("idle");
        setLaunchProgress(0);
        setLaunchStageLabel("Ready");
        setLaunchStageDetail(
          success
            ? "Minecraft exited normally."
            : `Minecraft exited with code ${exitCode ?? "unknown"}.`
        );
        // A game that dies a second after the spawn (no memory for -Xmx, a bad
        // JVM argument, a crash while loading) used to leave only a PLAY
        // button and a closed log (concorrenza-9). STOP is the user's own
        // exit, not one to report.
        if (!success && !stoppedByUser) {
          pushUiError({
            title: "Minecraft closed with an error",
            message: exitCode === null
              ? "The game was terminated without an exit code. The launch log shows what happened before it stopped."
              : `The game exited with code ${exitCode}. The launch log shows what went wrong.`,
            detail: "",
            severity: "error",
            scope: "launch",
            action: {
              label: "Show log",
              run: () => {
                setActiveRailView("modlist");
                setLogViewerOpen(true);
              },
            },
          });
        }
        // Il backend ha messo da parte il file prima dello spawn e risponde solo se esiste.
        invoke<string | null>("last_profile_recording_command")
          .then(setProfileRecordingPath)
          .catch((error) => logger.warn("App", "last_profile_recording_command failed", error));
      }),
    ]);

    // The launch lives in the backend, not in these signals: after a webview
    // reload the panel must show STOP over a running game, not a PLAY that
    // would start a second one on the same instance (concorrenza-4). Only
    // from "idle": an event that arrived first is newer than this reading.
    const restoreLaunchState = async () => {
      try {
        const status = await invoke<{ preparing: boolean; runningPid: number | null }>("launch_status_command");
        if (launchState() !== "idle") return;
        if (status.runningPid !== null) {
          setLaunchState("running");
          setLaunchProgress(100);
          setLaunchStageLabel("Minecraft running");
          setLaunchStageDetail(`Minecraft is running (PID ${status.runningPid}).`);
        } else if (status.preparing) {
          setLaunchState("resolving");
          setLaunchStageLabel("Preparing launch");
          setLaunchStageDetail("A launch started before the window was reloaded is still being prepared.");
        }
      } catch (error) {
        logger.warn("App", "launch_status_command failed", error);
      }
    };

    const boot = async () => {
      logger.info("App", "boot started");
      try {
        if (isTauri()) {
          unlisteners.push(...await registerListeners());
          await restoreLaunchState();
        }

        // Not awaited: Mojang's version list must never hold up the mod lists
        // and the accounts. It has its own timeout and falls back to disk.
        void loadMinecraftVersions();

        const snap = await loadShellSnapshot(null);
        logger.debug("App", "shell snapshot loaded", {
          modlists: (snap?.modlists ?? []).map((modlist: any) => modlist.name),
        });

        const firstList = snap?.modlists?.[0]?.name ?? "";
        if (firstList) {
          const [editorSnapshot] = await Promise.all([
            loadEditorSnapshot(firstList),
            loadModlistPresentation(firstList),
          ]);
          await loadModlistGroups(firstList, modRowsState());
          options.primePersistenceState();

          logger.debug("App", "boot completed", {
            firstList,
            rowCount: editorSnapshot?.rows?.length ?? modRowsState().length,
          });
          void fetchModMetadata(modRowsState());
          void runResolution(firstList, selectedMcVersion(), selectedModLoader());
        } else {
          logger.debug("App", "boot completed - no mod lists found");
        }
      } catch (error) {
        logger.error("App", "boot failed", error);
        pushUiError({
          title: "Launcher startup failed",
          message: "The launcher could not finish loading its initial data.",
          detail: String(error),
          severity: "error",
          scope: "launch",
        });
      }

      if (disposed) unlisteners.forEach((unlisten) => unlisten());
    };

    void boot();
    onCleanup(() => {
      disposed = true;
      unlisteners.forEach((unlisten) => unlisten());
    });
  });
}
