import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { onCleanup, onMount } from "solid-js";
import { logger } from "../lib/logger";
import {
  modRowsState,
  pushUiError,
  selectedMcVersion,
  selectedModLoader,
  setAppLoading,
  setLaunchLogs,
  setLaunchProgress,
  setLaunchStageDetail,
  setLaunchStageLabel,
  setLaunchState,
  setMcWithSnapshots,
  setMinecraftVersions,
  setProfileRecordingPath,
} from "../store";
import {
  fetchModMetadata,
  isTauri,
  loadEditorSnapshot,
  loadModlistGroups,
  loadModlistPresentation,
  loadShellSnapshot,
  markMinecraftVersionsKnown,
  runResolution,
} from "./backend-loaders";

interface AppBootstrapOptions {
  primePersistenceState: () => void;
}

export function useAppBootstrap(options: AppBootstrapOptions) {
  onMount(() => {
    let disposed = false;
    const unlisteners: Array<() => void> = [];

    const boot = async () => {
      logger.info("App", "boot started");
      try {
        if (isTauri()) {
          try {
            const payload: { releases: string[]; withSnapshots: string[] } = await invoke("fetch_minecraft_versions_command");
            logger.debug("App", "fetched minecraft versions", {
              releases: payload.releases.length,
              withSnapshots: payload.withSnapshots.length,
            });
            if (payload.releases.length > 0) {
              setMinecraftVersions(payload.releases);
              markMinecraftVersionsKnown();
            }
            if (payload.withSnapshots.length > 0) setMcWithSnapshots(payload.withSnapshots);
          } catch (error) {
            logger.warn("App", "failed to fetch minecraft versions, using defaults", error);
          }
        }

        const snap = await loadShellSnapshot(null);
        logger.debug("App", "shell snapshot loaded", {
          modlists: (snap?.modlists ?? []).map((modlist: any) => modlist.name),
        });

        const firstList = snap?.modlists?.[0]?.name ?? "";
        if (firstList) {
          const editorSnapshot = await loadEditorSnapshot(firstList);
          await loadModlistPresentation(firstList);
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

        if (isTauri()) {
          unlisteners.push(await listen<{ state: string; progress: number; stage: string; detail: string }>("launch-progress", (event) => {
            const { state, progress, stage, detail } = event.payload;
            setLaunchState(state as "idle" | "resolving" | "ready" | "running");
            setLaunchProgress(progress);
            setLaunchStageLabel(stage);
            setLaunchStageDetail(detail);
            // Un lancio nuovo: il profilo del precedente non è più quello da mostrare.
            if (state !== "idle") setProfileRecordingPath(null);
          }));

          unlisteners.push(await listen<{ stream: string; line: string }>("minecraft-log", (event) => {
            setLaunchLogs((current) => [...current, event.payload.line]);
          }));

          unlisteners.push(await listen<{ title: string; message: string; detail: string; severity?: "warning" | "error"; scope?: "launch" | "download" | "account" }>("launcher-error", (event) => {
            pushUiError({
              title: event.payload.title,
              message: event.payload.message,
              detail: event.payload.detail,
              severity: event.payload.severity ?? "error",
              scope: event.payload.scope ?? "launch",
            });
          }));

          unlisteners.push(await listen<{ success: boolean; exitCode: number | null }>("minecraft-exit", (event) => {
            setLaunchState("idle");
            setLaunchProgress(0);
            setLaunchStageLabel("Ready");
            setLaunchStageDetail(
              event.payload.success
                ? "Minecraft exited normally."
                : `Minecraft exited with code ${event.payload.exitCode ?? "unknown"}.`
            );
            // Il backend ha messo da parte il file prima dello spawn e risponde solo se esiste.
            invoke<string | null>("last_profile_recording_command")
              .then(setProfileRecordingPath)
              .catch((error) => logger.warn("App", "last_profile_recording_command failed", error));
          }));
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
      } finally {
        setAppLoading(false);
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
