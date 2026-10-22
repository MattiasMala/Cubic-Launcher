// Frontend orchestration root.
//
// This file wires backend commands, Tauri events, optimistic UI updates, and
// top-level modal actions. Feature-specific state and rendering should stay in
// `src/app/`, `src/store-*`, or colocated component folders instead of growing
// this file again.

import { Match, Show, Switch, batch, onMount } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { appendDebugTrace } from "./lib/debugTrace";
import { logger } from "./lib/logger";
import { updateAltsDeep } from "./lib/dragUtils";
import { CONTENT_TYPE_NOUN, describeContentImportError } from "./lib/contentImportErrors";
import type { LocalImportFailure } from "./lib/contentImportErrors";
import type { LocalPickMode, LocalUploadContentType } from "./components/add-mod-dialog/shared";

import {
  modListCards, setModListCards, selectedModListName, setSelectedModListName,
  activeRailView, setActiveRailView, setQuickPlayWorldFolder, quickPlayWorldFolder,
  modRowsState, setModRowsState, setActiveAccountId,
  setLaunchState, setLaunchProgress, setLaunchStageLabel, setLaunchStageDetail,
  setLaunchLogs,
  selectedMcVersion, setSelectedMcVersion, selectedModLoader, setSelectedModLoader,
  createModlistName, createModlistDescription, setCreateModlistModalOpen,
  setCreateModlistBusy, createModlistBusy,
  selectedIds, setSelectedIds, setExpandedRows, activeAccount, setAddModModalOpen,
  setSettingsModalOpen, localJarRuleName, setLocalJarRuleName,
  globalSettings, setGlobalSettings, setModlistOverrides,
  pushUiError, resetLaunchUiState,
  setAestheticGroups, setFunctionalGroups,
  setSavedIncompatibilities,
  setSavedLinks,
  draftIncompatibilities, setIncompatibilityModalOpen,
  instancePresentation, setInstancePresentation,
  exportOptions, setExportModalOpen, setInstancePresentationOpen,
  alternativesPanelParent, setAlternativesPanelParentId,
  setAdvancedPanelModId, rowMap, setOnToggleEnabled,
  setVersionRules, setCustomConfigs,
  launchState, selectedModList,
  setUpdateInfo,
  updateCheckRunning, setUpdateCheckRunning, pendingUpdatePrecheck, setPendingUpdatePrecheck,
  setContentLookupFailures,
  LAUNCH_STAGES, wait,
} from "./store";
import { normalizeModLoader, parseInstanceName, type ModRow, type UpdatePrecheckResult, type WorldInstanceLink } from "./lib/types";
import { buildResolvedContent, buildResolvedVersions } from "./lib/update-selection";
import { fetchContentProjects } from "./lib/content-meta";
import type { GlobalSettingsState, ModlistOverridesState, UpdateCheckResponse } from "./store";
import {
  buildIdRemap,
  collectRowIds,
  collectSubtreeIds,
  findRowIdByNamePath,
  findRowNamePath,
  rebuildRowIds,
  remapAestheticGroups,
  remapByRowId,
  remapFunctionalGroups,
  remapIds,
  remapIncompatibilities,
  remapLinks,
  removeRowsDeep,
  reorderAlternativesInRows,
  rowIdResolver,
  setEnabledDeep,
} from "./app/row-state";
import {
  defaultMinecraftVersion,
  fetchModMetadata,
  fetchMetadataForIds,
  seedModName,
  isTauri,
  loadEditorSnapshot,
  loadModlistGroups,
  loadModlistPresentation,
  loadShellSnapshot,
  modlistVersionLoaderCache,
  runResolution,
  saveModlistTarget,
} from "./app/backend-loaders";
import { useAppBootstrap } from "./app/use-app-bootstrap";
import { useAppPersistence } from "./app/persistence-effects";
import { Header } from "./components/Header";
import { UpdateBanner } from "./components/UpdateBanner";
import { NoticeBanner } from "./components/NoticeBanner";
import { Sidebar } from "./components/Sidebar";
import { ModListEditor } from "./components/ModListEditor";
import { ScreenshotsView } from "./components/ScreenshotsView";
import { HomeView } from "./components/HomeView";
import { SkinsScreen } from "./components/SkinsScreen";
import { bumpContentVersion, seedContentName } from "./components/mod-list-editor/ContentTabView";
import { LaunchPanel } from "./components/LaunchPanel";
import { AddModDialog } from "./components/AddModDialog";
import {
  CreateModlistModal,
  SettingsModal,
  AccountsModal,
  FunctionalGroupModal,
  LinkModal,
  LinksOverviewModal,
  InstancePresentationModal,
  IncompatibilitiesModal,
  AlternativesPanel,
  ErrorCenter,
  ExportModal,
  UpdatePopup,
} from "./components/Modals";
import { AdvancedModPanel } from "./components/AdvancedModPanel";
import { PinVersionDialog } from "./components/PinVersionDialog";

/**
 * The Advanced checkbox that governs one pre-check category.
 *
 * The three settings were written and read by nobody until now
 * (`app_shell.rs:59` said so in a comment); this table is the whole of
 * "reading" them. A category with no entry here — there is none today — is
 * treated as on, because silence must not hide an update.
 */
const CONTENT_CATEGORY_SETTINGS: Record<string, "updateNotificationsResourcePacks" | "updateNotificationsDataPacks" | "updateNotificationsShaders"> = {
  resourcepack: "updateNotificationsResourcePacks",
  datapack: "updateNotificationsDataPacks",
  shader: "updateNotificationsShaders",
};

function contentCategoryNotificationsEnabled(category: string): boolean {
  const setting = CONTENT_CATEGORY_SETTINGS[category];
  return setting ? globalSettings()[setting] : true;
}

export default function App() {
  const {
    markIncompatibilitiesSaved,
    primePersistenceState,
    resetPersistenceState,
  } = useAppPersistence();
  useAppBootstrap({ primePersistenceState });

  // ── Startup ────────────────────────────────────────────────────────────────
  onMount(() => {
    if (!isTauri()) return;

    void (async () => {
      try {
        const result = await invoke<UpdateCheckResponse>("check_for_updates");
        if (result.status === "available" && result.version) {
          setUpdateInfo({
            version: result.version,
            notes: result.notes,
            currentVersion: result.currentVersion,
          });
        }
      } catch {
        // Startup update checks are intentionally silent.
      }
    })();
  });


      // ── Register Tauri event listeners ──────────────────────────────────


  // ── Handlers ──────────────────────────────────────────────────────────────

  /**
   * Row ids are positional (`rule-<i>[-alternative-<n>]*-<name>`): after the
   * rows moved or some were deleted, rebuild them and carry every state keyed
   * by them along — groups, tags, links, incompatibilities, version rules,
   * custom configs, selection, panels. Entries of `removedIds` are dropped.
   * Without it the advanced save sent ids of rows that had moved, and the
   * backend refused every later save (audit 101, modlist-5, editor-logica-3).
   */
  const rebuildRowIdsAndRemap = (removedIds: ReadonlySet<string> = new Set()) => {
    const currentRows = modRowsState();
    const rebuiltRows = rebuildRowIds(currentRows);
    const resolve = rowIdResolver(buildIdRemap(currentRows, rebuiltRows), collectRowIds(rebuiltRows), removedIds);
    batch(() => {
      setModRowsState(rebuiltRows);
      setAestheticGroups(groups => remapAestheticGroups(groups, resolve));
      setFunctionalGroups(groups => remapFunctionalGroups(groups, resolve));
      setSavedIncompatibilities(rules => remapIncompatibilities(rules, resolve));
      setSavedLinks(links => remapLinks(links, resolve));
      setVersionRules(rules => remapByRowId(rules, resolve));
      setCustomConfigs(configs => remapByRowId(configs, resolve));
      setSelectedIds(ids => remapIds(ids, resolve));
      setExpandedRows(ids => remapIds(ids, resolve));
      setAlternativesPanelParentId(id => (id ? resolve(id) : id));
      setAdvancedPanelModId(id => (id ? resolve(id) : id));
    });
  };

  const handleSelectModList = async (name: string) => {
    logger.info("App", "handleSelectModList started", { name });
    appendDebugTrace("modlist.select", { name });
    // First: the persistence effects must not see the new name with the old
    // list's state, or they save it there.
    resetPersistenceState();
    setSelectedModListName(name);
    setSelectedIds([]);

    // Immediately show cached presentation data so there's no flicker
    const cached = modListCards().find(c => c.name === name);
    if (cached?.iconLabel !== undefined) {
      setInstancePresentation(cur => ({
        ...cur,
        displayName: cached.displayName ?? cur.displayName,
        iconLabel: cached.iconLabel ?? cur.iconLabel,
        iconAccent: cached.iconAccent ?? cur.iconAccent,
        iconImage: cached.iconImage ?? cur.iconImage,
      }));
    }
    // Restore cached version/loader immediately — prevents bleed from previous modlist
    const cachedVL = modlistVersionLoaderCache.get(name);
    if (cachedVL) { setSelectedMcVersion(cachedVL.version); setSelectedModLoader(normalizeModLoader(cachedVL.loader)); }
    if (!isTauri()) { logger.warn("App", "handleSelectModList skipped — no backend"); return; }
    await loadShellSnapshot(name);
    await loadEditorSnapshot(name, true);
    await loadModlistPresentation(name);
    await loadModlistGroups(name, modRowsState());
    primePersistenceState();

    void fetchModMetadata(modRowsState());
    void runResolution(name, selectedMcVersion(), selectedModLoader());
  };

  const handleReorderRules = async (orderedIds: string[]) => {
    logger.info("App", "handleReorderRules started", { count: orderedIds.length });
    if (!isTauri() || !selectedModListName()) { logger.warn("App", "handleReorderRules skipped — no backend"); return; }
    try {
      await invoke("reorder_rules_command", {
        input: { modlistName: selectedModListName(), orderedModIds: orderedIds.map(id => rowMap().get(id)?.primaryModId ?? id) },
      });
      rebuildRowIdsAndRemap();
      void runResolution();
    } catch (err) {
      logger.error("App", "handleReorderRules failed", err);
      pushUiError({ title: "Reorder failed", message: "The new rule order could not be saved.", detail: String(err), severity: "error", scope: "launch" });
      await loadEditorSnapshot(selectedModListName()); // restore original order on failure
    }
  };

  const handleSaveAlternativeOrder = async (parentId: string, orderedAltIds: string[]) => {
    const modlistName = selectedModListName();
    if (!modlistName) return;
    appendDebugTrace("alts.reorder.frontend", { parentId, orderedAltIds, modlistName });

    // The mod ids are read before the row ids are rebuilt below.
    const parentModId = rowMap().get(parentId)?.primaryModId ?? parentId;
    const orderedAltModIds = orderedAltIds.map(id => rowMap().get(id)?.primaryModId ?? id);

    // Optimistic: reorder alternatives in local state immediately, at any depth,
    // then renumber their positional ids so tags and groups follow them.
    setModRowsState(cur => reorderAlternativesInRows(cur, parentId, orderedAltIds));
    rebuildRowIdsAndRemap();

    if (!isTauri()) { logger.warn("App", "handleSaveAlternativeOrder skipped — no backend"); return; }
    try {
      await invoke("save_alternative_order_command", {
        input: { modlistName, parentModId, orderedAltIds: orderedAltModIds },
      });
      void runResolution();
      appendDebugTrace("alts.reorder.frontend", { parentId, status: "saved" });
    } catch (err) {
      appendDebugTrace("alts.reorder.frontend", { parentId, status: "error", detail: String(err) });
      pushUiError({ title: "Could not save alternative order", message: "The fallback order could not be persisted.", detail: String(err), severity: "error", scope: "launch" });
      // Restore on failure: the order on disk, and the groups that follow it.
      await loadEditorSnapshot(modlistName);
      await loadModlistGroups(modlistName, modRowsState());
    }
  };

  const handleAddAlternative = async (parentId: string, altRowId: string) => {
    if (!isTauri()) {
      const parentRow = rowMap().get(parentId);
      const altRow = rowMap().get(altRowId);
      if (!parentRow || !altRow) return;

      // Remove altRow from wherever it currently lives (top-level or nested)
      const withoutAlt = modRowsState()
        .filter(r => r.id !== altRowId)
        .map(r => ({
          ...r,
          alternatives: (r.alternatives ?? []).filter(a => a.id !== altRowId),
        }));

      // Add a shallow copy as alternative of parent (breaks the shared reference)
      setModRowsState(
        updateAltsDeep(withoutAlt, parentId, [
          ...(parentRow.alternatives ?? []),
          { ...altRow, alternatives: [] },
        ])
      );
      return;
    }
    if (!selectedModListName()) { logger.warn("App", "handleAddAlternative skipped — no modlist"); return; }
    // If the parent is itself an alternative (contains "-alternative-"),
    // use the nested alternative command; otherwise use the flat command.
    const isNested = parentId.includes("-alternative-");
    const parentNamePath = findRowNamePath(modRowsState(), parentId);
    appendDebugTrace("alts.add.frontend", {
      modlistName: selectedModListName(),
      parentId,
      altRowId,
      isNested,
      parentNamePath,
    });
    const altModId = rowMap().get(altRowId)?.primaryModId ?? altRowId;
    const parentModId = rowMap().get(parentId)?.primaryModId ?? parentId;
    const altSource = rowMap().get(altRowId)?.kind ?? "modrinth";
    try {
      // Move the mod to be an alternative of the parent. The backend extracts
      // the rule from its current position (preserving all data) and re-inserts
      // it as an alternative, so no separate delete is needed.
      if (isNested) {
        await invoke("add_nested_alternative_command", {
          input: { modlistName: selectedModListName(), parentModId, modId: altModId, source: altSource },
        });
      } else {
        await invoke("add_alternative_command", {
          input: { modlistName: selectedModListName(), parentModId, modId: altModId, source: altSource },
        });
      }
      await loadEditorSnapshot(selectedModListName());
      await loadModlistGroups(selectedModListName(), modRowsState());
      if (parentNamePath) {
        const nextParentId = findRowIdByNamePath(modRowsState(), parentNamePath);
        if (nextParentId) setAlternativesPanelParentId(nextParentId);
        appendDebugTrace("alts.add.frontend", {
          status: "reloaded",
          parentId,
          nextParentId,
          parentNamePath,
        });
      }
      void fetchModMetadata(modRowsState());
    } catch (err) {
      appendDebugTrace("alts.add.frontend", { status: "error", parentId, altRowId, detail: String(err) });
      pushUiError({ title: "Could not add alternative", message: "The mod could not be added as a fallback.", detail: String(err), severity: "error", scope: "launch" });
    }
  };

  const handleRemoveAlternative = async (altRowId: string) => {
    if (!isTauri()) {
      setModRowsState(rows => {
        // Find and extract the alt, then promote it to its parent's level.
        let extracted: ModRow | null = null;
        let parentId: string | null = null;
        const withoutAlt = rows.map(function removeDeep(r: ModRow): ModRow {
          if (!r.alternatives?.length) return r;
          const altIdx = r.alternatives.findIndex(a => a.id === altRowId);
          if (altIdx !== -1) {
            extracted = r.alternatives[altIdx];
            parentId = r.id;
            const filtered = [...r.alternatives.slice(0, altIdx), ...r.alternatives.slice(altIdx + 1)].map(removeDeep);
            return { ...r, alternatives: filtered };
          }
          return { ...r, alternatives: r.alternatives.map(removeDeep) };
        });
        if (!extracted) return withoutAlt;
        // Insert after parent at the same level
        const topIdx = withoutAlt.findIndex(r => r.id === parentId);
        if (topIdx !== -1) {
          return [...withoutAlt.slice(0, topIdx + 1), extracted, ...withoutAlt.slice(topIdx + 1)];
        }
        // Parent is nested — insert as sibling alt (simplified: append to top for now)
        return [...withoutAlt, extracted];
      });
      return;
    }
    if (!selectedModListName()) { logger.warn("App", "handleRemoveAlternative skipped — no modlist"); return; }
    // Save the panel parent's stable mod_id so we can restore it after reload.
    const panelParentModId = alternativesPanelParent()?.primaryModId ?? null;
    appendDebugTrace("alts.remove.frontend", {
      modlistName: selectedModListName(),
      altRowId,
      activePanelModId: panelParentModId,
    });
    try {
      const altModId = rowMap().get(altRowId)?.primaryModId ?? altRowId;
      const findParent = (rows: ModRow[]): ModRow | undefined => {
        for (const r of rows) {
          if (r.alternatives?.some(a => a.id === altRowId)) return r;
          if (r.alternatives?.length) { const found = findParent(r.alternatives); if (found) return found; }
        }
        return undefined;
      };
      const parentRow = findParent(modRowsState());
      const parentModId = parentRow?.primaryModId ?? "";
      await invoke("remove_alternative_command", {
        input: { modlistName: selectedModListName(), parentModId, altModId },
      });
      await loadEditorSnapshot(selectedModListName());
      await loadModlistGroups(selectedModListName(), modRowsState());
      // Restore the alternatives panel using the stable mod_id.
      if (panelParentModId) {
        const nextParent = [...rowMap().values()].find(r => r.primaryModId === panelParentModId);
        if (nextParent) setAlternativesPanelParentId(nextParent.id);
        else setAlternativesPanelParentId(null);
        appendDebugTrace("alts.remove.frontend", {
          status: "reloaded",
          altRowId,
          nextParentId: nextParent?.id ?? null,
          panelParentModId,
        });
      }
    } catch (err) {
      appendDebugTrace("alts.remove.frontend", { status: "error", altRowId, detail: String(err) });
      pushUiError({ title: "Could not remove alternative", message: "The mod could not be detached as a fallback.", detail: String(err), severity: "error", scope: "launch" });
    }
  };

  const handleSwitchAccount = async (id: string) => {
    setActiveAccountId(id);
    if (!isTauri()) { logger.warn("App", "handleSwitchAccount skipped — no backend"); return; }
    try {
      await invoke("switch_active_account_command", { microsoftId: id });
      await loadShellSnapshot(selectedModListName() || null);
    } catch (err) {
      pushUiError({ title: "Account switch failed", message: "Could not activate the selected account.", detail: String(err), severity: "error", scope: "account" });
    }
  };

  const handleCreateModlist = async () => {
    const name = createModlistName().trim();
    if (!name || createModlistBusy()) return;
    logger.info("App", "handleCreateModlist started", { name });
    setCreateModlistBusy(true);

    if (!isTauri()) {
      logger.warn("App", "handleCreateModlist skipped — no backend");
      // Browser mode: add to the list locally so the UI is usable
      setModListCards(cur => [...cur, { name, status: "Ready", accent: "from-primary/30 via-primary/10 to-transparent", description: createModlistDescription().trim() || "New mod list." }]);
      setSelectedModListName(name);
      setModRowsState([]);
      setCreateModlistModalOpen(false);
      setCreateModlistBusy(false);
      return;
    }

    // The target a new list starts on is written with it, so it never follows
    // the next Mojang release (audit 101, r2-primo-avvio-ambiente-degradato-1).
    const minecraftVersion = defaultMinecraftVersion();
    const modLoader = normalizeModLoader(null);
    try {
      await invoke("create_modlist_command", {
        input: {
          name,
          author: activeAccount()?.gamertag ?? "Author",
          description: createModlistDescription().trim(),
          minecraftVersion,
          modLoader,
        },
      });
      setCreateModlistModalOpen(false);
      // Also saved on its own: it does not depend on the create command
      // knowing the two fields.
      try {
        await saveModlistTarget(name, minecraftVersion, modLoader);
      } catch (err) {
        pushUiError({ title: "Version not saved", message: `'${name}' was created, but its Minecraft version and loader could not be saved.`, detail: String(err), severity: "warning", scope: "launch" });
      }
      // The same path as picking a list in the sidebar: the persistence state
      // of the previous list is dropped before anything of the new one loads
      // (audit 101, contratto-4).
      await handleSelectModList(name);

      logger.debug("App", "handleCreateModlist completed", { name });
    } catch (err) {
      logger.error("App", "handleCreateModlist failed", err);
      pushUiError({ title: "Failed to create mod list", message: `'${name}' could not be created.`, detail: String(err), severity: "error", scope: "launch" });
    } finally {
      setCreateModlistBusy(false);
    }
  };

  const handleAddModrinth = async (modId: string, displayName: string) => {
    // Must have a mod list selected before we can add anything
    if (!selectedModListName()) {
      pushUiError({ title: "No mod list selected", message: "Create or select a mod list in the sidebar before adding mods.", detail: "", severity: "warning", scope: "launch" });
      return;
    }
    logger.info("App", "handleAddModrinth started", { modId, displayName });
    // Optimistic insert: append a placeholder row immediately so the list
    // never fully re-renders.  The backend normalises the rule name the same
    // way we compute the ID here, so the reference is stable after the reload.
    const tempId = `rule-${modRowsState().length}-${modId}`;
    const tempRow: ModRow = {
      id: tempId,
      name: displayName,
      modrinth_id: modId,
      kind: "modrinth",
      enabled: true,
      area: "Rule",
      note: "Primary option with 1 mod.",
      tags: [],
      alternatives: [],
    };
    setModRowsState(c => [...c, tempRow]);
    // The dialog already knows the human-readable title; seed the name cache
    // so the card keeps it after the snapshot reload even if the Modrinth
    // metadata fetch fails.
    seedModName(modId, displayName);
    void fetchModMetadata([tempRow]);

    if (!isTauri()) { logger.warn("App", "handleAddModrinth skipped — no backend"); return; }
    try {
      await invoke("add_mod_rule_command", {
        input: { modlistName: selectedModListName(), modId, source: "modrinth", fileName: null },
      });
      // Smart-merge: only the new row changes (or its ID gets corrected); existing rows keep references.
      await loadEditorSnapshot(selectedModListName());
      logger.debug("App", "handleAddModrinth completed", { modId, displayName });
    } catch (err) {
      logger.error("App", "handleAddModrinth failed", err);
      // Remove the optimistic row on failure.
      setModRowsState(c => c.filter(r => r.id !== tempId));
      pushUiError({ title: "Failed to add mod", message: `'${displayName}' could not be added.`, detail: String(err), severity: "error", scope: "launch" });
    }
  };

  /**
   * Copy a local pack into the mod list (`import_local_content_pack_command`).
   *
   * Returns the refusal instead of pushing a notice: the Add dialog covers the
   * notice banner, so a message pushed there would be invisible exactly when
   * it matters. The dialog shows what comes back.
   */
  const importLocalContentPack = async (
    sourcePath: string,
    contentType: LocalUploadContentType,
  ): Promise<LocalImportFailure | null> => {
    const modlistName = selectedModListName();
    if (!modlistName) return null;
    try {
      await invoke("import_local_content_pack_command", {
        input: { modlistName, contentType, sourcePath },
      });
      setAddModModalOpen(false);
      // The command hands back the row it created, but reloading the list is
      // the existing "the content list changed" path and also picks up groups
      // and order. One command, one reload.
      bumpContentVersion();
      logger.debug("App", "importLocalContentPack completed", { contentType, sourcePath });
      return null;
    } catch (err) {
      logger.error("App", "importLocalContentPack failed", err);
      return describeContentImportError(err, { modlistName, contentType, sourcePath });
    }
  };

  /**
   * The Browse buttons of the Upload tab.
   *
   * A mod is a `.jar` and keeps the local-JAR command. A pack is a `.zip` or a
   * folder, and since a Tauri dialog opens files or directories but not both,
   * `pick` says which of the two buttons was pressed.
   */
  const handleUploadLocal = async (
    contentType: LocalUploadContentType,
    pick: LocalPickMode,
  ): Promise<LocalImportFailure | null> => {
    if (!isTauri()) {
      pushUiError({
        title: "Desktop app required",
        message: contentType === "mod"
          ? "Local JAR upload requires the Cubic Launcher desktop app."
          : "Importing a local pack requires the Cubic Launcher desktop app.",
        detail: "The file picker is only available when running inside Tauri.",
        severity: "warning",
        scope: "launch",
      });
      return null;
    }
    const { open } = await import("@tauri-apps/plugin-dialog");
    if (contentType === "mod") {
      try {
        const selected = await open({ title: "Select a mod JAR", filters: [{ name: "JAR files", extensions: ["jar"] }], multiple: false, directory: false });
        if (!selected || !selectedModListName()) return null;
        await invoke("copy_local_jar_command", {
          input: {
            sourcePath: selected as string,
            ruleName: localJarRuleName().trim(),
            modlistName: selectedModListName(),
          },
        });
        setLocalJarRuleName("");
        setAddModModalOpen(false);
        await loadEditorSnapshot(selectedModListName());
      } catch (err) {
        pushUiError({ title: "Failed to upload JAR", message: "The file could not be copied to the mod cache.", detail: String(err), severity: "error", scope: "launch" });
      }
      return null;
    }

    const noun = CONTENT_TYPE_NOUN[contentType] ?? "pack";
    try {
      const selected = pick === "directory"
        ? await open({ title: `Select an unpacked ${noun} folder`, multiple: false, directory: true })
        : await open({ title: `Select a ${noun} ZIP`, filters: [{ name: "ZIP archives", extensions: ["zip"] }], multiple: false, directory: false });
      if (!selected) return null;
      return await importLocalContentPack(selected as string, contentType);
    } catch (err) {
      logger.error("App", "handleUploadLocal picker failed", err);
      return { title: `The ${noun} picker could not be opened`, message: "The file dialog did not open, so nothing was imported.", detail: String(err) };
    }
  };

  /**
   * The drop zone of the Upload tab. A dropped `.jar` is a mod; for a pack the
   * dropped path is a `.zip` or the folder of an unpacked pack, and the
   * backend is the side that tells those apart from a stray file.
   */
  const handleDropLocal = async (
    path: string,
    contentType: LocalUploadContentType,
  ): Promise<LocalImportFailure | null> => {
    if (!selectedModListName()) return null;
    if (contentType !== "mod") return await importLocalContentPack(path, contentType);
    try {
      await invoke("copy_local_jar_command", {
        input: { sourcePath: path, ruleName: localJarRuleName().trim(), modlistName: selectedModListName() },
      });
      setLocalJarRuleName("");
      setAddModModalOpen(false);
      await loadEditorSnapshot(selectedModListName());
    } catch (err) {
      pushUiError({ title: "Failed to upload JAR", message: "The dropped file could not be added.", detail: String(err), severity: "error", scope: "launch" });
    }
    return null;
  };

  const handleDeleteSelected = async () => {
    const ids = selectedIds();
    if (ids.length === 0) return;
    logger.info("App", "handleDeleteSelected started", { ids });
    const idSet = new Set(ids);

    // Capture real mod IDs BEFORE the optimistic removal clears them from rowMap.
    const backendModIds = ids.map(id => rowMap().get(id)?.primaryModId ?? id);
    // Every row that goes away: the selected ones and what sits below them.
    const removedIds = new Set<string>();
    for (const id of ids) {
      const row = rowMap().get(id);
      if (row) collectSubtreeIds(row, removedIds);
      else removedIds.add(id);
    }

    // Optimistic update: remove the rows at any depth, as the backend does.
    setModRowsState(cur => removeRowsDeep(cur, idSet));
    setSelectedIds([]);
    if (!isTauri() || !selectedModListName()) { logger.warn("App", "handleDeleteSelected skipped — no backend"); return; }
    try {
      await invoke("delete_rules_command", { input: { modlistName: selectedModListName(), modIds: backendModIds } });
      rebuildRowIdsAndRemap(removedIds);
      void runResolution();
      logger.debug("App", "handleDeleteSelected completed");
    } catch (err) {
      logger.error("App", "handleDeleteSelected failed", err);
      pushUiError({ title: "Failed to delete rules", message: "The selected rules could not be removed.", detail: String(err), severity: "error", scope: "launch" });
      await loadEditorSnapshot(selectedModListName()); // restore on failure
    }
  };

  const handleSaveSettings = async (globalDraft: GlobalSettingsState, modlistDraft: ModlistOverridesState) => {
    logger.info("App", "handleSaveSettings started");
    if (!isTauri()) {
      logger.warn("App", "handleSaveSettings skipped — no backend");
      setGlobalSettings({ ...globalDraft });
      setModlistOverrides({ ...modlistDraft });
      setSettingsModalOpen(false);
      return;
    }
    try {
      await invoke("save_global_settings_command", {
        settings: {
          minRamMb: globalDraft.minRamMb,
          maxRamMb: globalDraft.maxRamMb,
          customJvmArgs: globalDraft.customJvmArgs,
          profilerEnabled: globalDraft.profilerEnabled,
          updateNotificationsEnabled: globalDraft.updateNotificationsEnabled,
          updateNotificationsResourcePacks: globalDraft.updateNotificationsResourcePacks,
          updateNotificationsDataPacks: globalDraft.updateNotificationsDataPacks,
          updateNotificationsShaders: globalDraft.updateNotificationsShaders,
          wrapperCommand: globalDraft.wrapperCommand,
          javaPathOverride: globalDraft.javaPathOverride,
        },
      });
      if (selectedModListName()) {
        await invoke("save_modlist_overrides_command", {
          overrides: {
            modlistName: selectedModListName(),
            minRamMb: modlistDraft.minRamEnabled ? modlistDraft.minRamMb : null,
            maxRamMb: modlistDraft.maxRamEnabled ? modlistDraft.maxRamMb : null,
            customJvmArgs: modlistDraft.customArgsEnabled ? modlistDraft.customJvmArgs : null,
            profilerEnabled: modlistDraft.profilerEnabled ? modlistDraft.profilerActive : null,
            wrapperCommand: modlistDraft.wrapperEnabled ? modlistDraft.wrapperCommand : null,
            minecraftVersion: selectedMcVersion() || null,
            modLoader: normalizeModLoader(selectedModLoader()) || null,
          },
        });
      }
      setGlobalSettings({ ...globalDraft });
      setModlistOverrides({ ...modlistDraft });
      setSettingsModalOpen(false);
      await loadShellSnapshot(selectedModListName() || null);
      logger.debug("App", "handleSaveSettings completed");
    } catch (err) {
      logger.error("App", "handleSaveSettings failed", err);
      pushUiError({ title: "Settings not saved", message: "The settings could not be written to disk.", detail: String(err), severity: "error", scope: "launch" });
    }
  };

  /**
   * Write the version and loader just picked. Only those two keys: the RAM,
   * JVM and wrapper overrides on screen may still be another list's, and
   * sending them copied them onto this one (todo.md:1618). A failed save is
   * shown and the previous pick comes back, instead of the dropdown claiming a
   * target that the next reload forgets (audit 101 "Noto 1", 069 M3).
   */
  const saveVersionLoader = async (
    modlistName: string,
    version: string,
    loader: string,
    previous: { version: string; loader: string },
  ) => {
    if (!isTauri() || !modlistName) { logger.warn("App", "saveVersionLoader skipped — no backend"); return; }
    try {
      await saveModlistTarget(modlistName, version, loader);
    } catch (err) {
      logger.error("App", "saveVersionLoader failed", err);
      pushUiError({ title: "Version not saved", message: `The Minecraft version and loader of '${modlistName}' could not be saved, so the previous ones are back.`, detail: String(err), severity: "error", scope: "launch" });
      // Undo only what this save set: a later pick has its own save.
      const cached = modlistVersionLoaderCache.get(modlistName);
      if (cached?.version !== version || cached.loader !== loader) return;
      modlistVersionLoaderCache.set(modlistName, previous);
      setModListCards(cards => cards.map(c => c.name === modlistName ? { ...c, mcVersion: previous.version, modLoader: previous.loader } : c));
      if (selectedModListName() === modlistName) {
        setSelectedMcVersion(previous.version);
        setSelectedModLoader(previous.loader);
        void runResolution(modlistName, previous.version, previous.loader);
      }
    }
  };

  const handleVersionChange = (version: string) => {
    const modlistName = selectedModListName();
    const loader = normalizeModLoader(selectedModLoader());
    const previous = { version: selectedMcVersion(), loader };
    setSelectedMcVersion(version);
    modlistVersionLoaderCache.set(modlistName, { version, loader });
    setModListCards(cards => cards.map(c => c.name === modlistName ? { ...c, mcVersion: version } : c));
    void saveVersionLoader(modlistName, version, loader, previous);
    void runResolution(modlistName, version, loader);
  };

  const handleLoaderChange = (loader: string) => {
    const modlistName = selectedModListName();
    const normalizedLoader = normalizeModLoader(loader);
    const previous = { version: selectedMcVersion(), loader: normalizeModLoader(selectedModLoader()) };
    setSelectedModLoader(normalizedLoader);
    modlistVersionLoaderCache.set(modlistName, { version: selectedMcVersion(), loader: normalizedLoader });
    setModListCards(cards => cards.map(c => c.name === modlistName ? { ...c, modLoader: normalizedLoader } : c));
    void saveVersionLoader(modlistName, selectedMcVersion(), normalizedLoader, previous);
    void runResolution(modlistName, selectedMcVersion(), normalizedLoader);
  };

  const handleSaveIncompatibilities = async () => {
    // Keep synthetic IDs for the frontend signals (UI uses them)
    const rules = draftIncompatibilities().map(rule => ({
      winnerId: rule.winnerId,
      loserId: rule.loserId,
    }));
    // Map to real mod IDs for the backend
    const backendRules = rules.map(rule => ({
      winnerId: rowMap().get(rule.winnerId)?.primaryModId ?? rule.winnerId,
      loserId: rowMap().get(rule.loserId)?.primaryModId ?? rule.loserId,
    }));

    if (!isTauri() || !selectedModListName()) {
      setSavedIncompatibilities(rules.map(rule => ({ ...rule })));
      markIncompatibilitiesSaved(rules);
      setIncompatibilityModalOpen(false);
      return;
    }

    try {
      await invoke("save_incompatibilities_command", {
        input: {
          modlistName: selectedModListName(),
          rules: backendRules,
        },
      });
      setSavedIncompatibilities(rules.map(rule => ({ ...rule })));
      markIncompatibilitiesSaved(rules);
      setIncompatibilityModalOpen(false);
      await loadEditorSnapshot(selectedModListName());
    } catch (err) {
      pushUiError({ title: "Could not save incompatibilities", message: "The incompatibility rules could not be persisted.", detail: String(err), severity: "error", scope: "launch" });
    }
  };

  const handleToggleEnabled = async (rowId: string | string[], enabled: boolean) => {
    const ids = Array.isArray(rowId) ? rowId : [rowId];
    const modIds = ids.map(id => rowMap().get(id)?.primaryModId ?? id);
    const modlistName = selectedModListName();
    if (!isTauri() || !modlistName) return;

    // Lock the scroll position across the optimistic re-render and the save,
    // which is a local file write. Not across the resolution: that one can hit
    // the network, and the list stayed frozen until it returned (audit 101,
    // stato-frontend-9).
    const scrollEl = document.querySelector<HTMLElement>(".flex-1.p-4");
    const savedScroll = scrollEl?.scrollTop ?? 0;
    let scrollGuardId = 0;
    const guardScroll = () => { if (scrollEl) scrollEl.scrollTop = savedScroll; scrollGuardId = requestAnimationFrame(guardScroll); };
    scrollGuardId = requestAnimationFrame(guardScroll);

    // Optimistic update — the target rows at any depth (an alternative has its
    // own switch too) and everything below them, as the backend does.
    setModRowsState(cur => setEnabledDeep(cur, new Set(ids), enabled));
    let saved = false;
    try {
      // One command for the whole set: a group toggle used to be one full
      // read and write of rules.json per mod (r2-scala-mod-list-grandi-4).
      await invoke("toggle_rules_enabled_command", { input: { modlistName, modIds, enabled } });
      saved = true;
    } catch (err) {
      pushUiError({ title: "Toggle failed", message: `Could not ${enabled ? "enable" : "disable"} the mod(s).`, detail: String(err), severity: "error", scope: "launch" });
      await loadEditorSnapshot(modlistName);
    } finally {
      cancelAnimationFrame(scrollGuardId);
      if (scrollEl) scrollEl.scrollTop = savedScroll;
    }
    if (saved) void runResolution();
  };

  setOnToggleEnabled(() => handleToggleEnabled);

  const handleSavePresentation = async () => {
    if (!selectedModListName()) return;

    if (!isTauri()) {
      setInstancePresentationOpen(false);
      return;
    }

    try {
      const presentation = instancePresentation();
      await invoke("save_modlist_presentation_command", {
        input: {
          modlistName: selectedModListName(),
          displayName: presentation.displayName || null,
          iconLabel: presentation.iconLabel,
          iconAccent: presentation.iconAccent,
          notes: presentation.notes,
          iconImage: presentation.iconImage || null,
        },
      });
      setInstancePresentationOpen(false);
      await loadModlistPresentation(selectedModListName());
    } catch (err) {
      pushUiError({ title: "Could not save settings", message: "The mod-list settings could not be persisted.", detail: String(err), severity: "error", scope: "launch" });
    }
  };

  const handleDeleteModList = async () => {
    const name = selectedModListName();
    if (!name) return;

    if (!isTauri()) {
      setInstancePresentationOpen(false);
      return;
    }

    try {
      await invoke("delete_modlist_command", { modlistName: name });
      // The deleted list's tags, groups and rules must not be saved under the
      // next one's name (audit 101, ui-schermate-1).
      resetPersistenceState();
      setInstancePresentationOpen(false);
      modlistVersionLoaderCache.delete(name);
      const next = modListCards().find(card => card.name !== name)?.name ?? null;
      if (next) {
        // The same path as the sidebar: shell snapshot (cards, target and
        // overrides of the next list), editor, presentation, groups.
        await handleSelectModList(next);
      } else {
        await loadShellSnapshot(null);
        batch(() => {
          setInstancePresentation({ displayName: "", iconLabel: "ML", iconAccent: "", notes: "", iconImage: "" });
          setModRowsState([]);
          setFunctionalGroups([]);
          setAestheticGroups([]);
          setSavedLinks([]);
          setSavedIncompatibilities([]);
          setVersionRules([]);
          setCustomConfigs([]);
          setExpandedRows([]);
          setSelectedIds([]);
        });
        void runResolution("");
      }
    } catch (err) {
      pushUiError({ title: "Could not delete mod-list", message: "The mod-list could not be deleted.", detail: String(err), severity: "error", scope: "launch" });
    }
  };

  const handleExport = async () => {
    if (!selectedModListName()) return;

    if (!isTauri()) {
      pushUiError({ title: "Desktop app required", message: "Export requires the Cubic Launcher desktop app.", detail: "The native save dialog is only available inside Tauri.", severity: "warning", scope: "launch" });
      return;
    }

    try {
      const { save } = await import("@tauri-apps/plugin-dialog");
      const destination = await save({
        title: "Export Mod List",
        defaultPath: `${selectedModListName()}.zip`,
        filters: [{ name: "ZIP archive", extensions: ["zip"] }],
      });
      if (!destination) return;

      const options = exportOptions();
      await invoke("export_modlist_command", {
        input: {
          modlistName: selectedModListName(),
          destinationPath: destination,
          rulesJson: options.rulesJson,
          configFiles: options.configFiles,
          resourcePacks: options.resourcePacks,
          dataPacks: options.dataPacks,
          shaders: options.shaders,
          otherFiles: options.otherFiles,
          selectedOtherPaths: options.selectedOtherPaths,
        },
      });
      setExportModalOpen(false);
    } catch (err) {
      pushUiError({ title: "Export failed", message: "The mod-list archive could not be created.", detail: String(err), severity: "error", scope: "launch" });
    }
  };

  const startLaunch = async (
    resolvedVersions?: Record<string, string>,
    resolvedContent?: Record<string, Record<string, string>>,
  ) => {
    const quickPlayFolder = quickPlayWorldFolder();
    try {
      await invoke("start_launch_command", {
        request: {
          modlistName: selectedModListName(),
          minecraftVersion: selectedMcVersion(),
          modLoader: normalizeModLoader(selectedModLoader()),
          // Omitted, not `undefined`: a request without the field is the
          // pre-feature request, and the backend reads its absence (D19).
          ...(resolvedVersions ? { resolvedVersions } : {}),
          ...(resolvedContent ? { resolvedContent } : {}),
          // Same rule for the world a "Jump in" Play asked for: absent means
          // the ordinary launch, which stops at the menu.
          ...(quickPlayFolder ? { quickPlaySingleplayer: quickPlayFolder } : {}),
        },
      });
    } catch (err) {
      pushUiError({ title: "Launch failed", message: "The backend could not start the launch.", detail: String(err), severity: "error", scope: "launch" });
    } finally {
      // One launch, one world: a second Play from the panel must not inherit
      // the world the card asked for.
      setQuickPlayWorldFolder(null);
    }
  };

  /**
   * `worldFolder` is the world this launch should open, and defaults to none:
   * every launch says what it wants instead of inheriting what the last one
   * asked for. It matters with real data — both instances hold a folder
   * called `New World`, so a stale value would not fail, it would jump into
   * the wrong world.
   */
  const handleLaunch = async (worldFolder: string | null = null) => {
    if (launchState() === "resolving" || launchState() === "running" || updateCheckRunning()) return;
    setQuickPlayWorldFolder(worldFolder);
    if (!selectedModList()) {
      pushUiError({ title: "No mod list selected", message: "Select a mod list from the sidebar before launching.", detail: "", severity: "warning", scope: "launch" });
      return;
    }
    resetLaunchUiState();

    if (isTauri()) {
      // The pre-check runs before every launch: making it conditional on a
      // setting is A3. Two commands instead of one widen the double-click
      // window, hence the guard above and the disabled Play button.
      setUpdateCheckRunning(true);
      let precheck: UpdatePrecheckResult;
      try {
        precheck = await invoke<UpdatePrecheckResult>("update_precheck_command", {
          request: {
            modlistName: selectedModListName(),
            minecraftVersion: selectedMcVersion(),
            modLoader: normalizeModLoader(selectedModLoader()),
          },
        });
      } catch (err) {
        // D19: an update check must never be the reason the game does not
        // start. No map means the launch picks versions exactly as it did
        // before this feature existed.
        pushUiError({ title: "Update check failed", message: "Launching with the versions already in your cache.", detail: String(err), severity: "warning", scope: "launch" });
        setLaunchLogs(current => [...current, `[Launcher] WARN update check failed, launching from the cached versions — ${String(err)}`]);
        await startLaunch();
        return;
      } finally {
        setUpdateCheckRunning(false);
      }

      // Icons and names are best-effort and reactive: the popup opens now and
      // fills in when the single request lands. The two halves have two
      // caches: mods by project id, packs by entry id.
      void fetchMetadataForIds(precheck.updates.map(update => update.projectId));
      void fetchContentProjects(precheck.contentUpdates.map(update => update.entryId));

      // D63 reaches the editor row from here, not from a second request: the
      // popup only opens when there is an update, and a pack nobody could ask
      // about may well produce none. Rewritten every pre-check, so a failure
      // that goes away stops being reported.
      setContentLookupFailures(
        new Map(precheck.contentLookupFailures.map(failure => [`${failure.category}/${failure.entryId}`, failure.error])),
      );

      // The three Advanced checkboxes, finally read. A category that is off
      // keeps its rows out of the popup **and** out of the accepted set, so
      // its entries stay at the version on disk — D30's rule, per category
      // instead of globally. An entry that was never installed has no row, so
      // it keeps its candidate and is installed anyway: switching a category
      // off means "do not update", never "do not install" (D17).
      const shownContentUpdates = precheck.contentUpdates.filter(row =>
        contentCategoryNotificationsEnabled(row.category),
      );

      // D30: with notifications off the pre-check still ran — a mod that was
      // never downloaded has to be resolved or it would never be installed —
      // but no update is shown and nothing is updated. `resolved` carries the
      // **candidate** versions, so handing it over as it is would update
      // every mod in silence, which is the opposite of what switching the
      // notifications off asks for. An empty accepted set is the same code
      // path as the popup's "Skip".
      //
      // D116: mods the launch will leave out are not updates, so that setting
      // does not silence them. With some, the popup opens anyway and shows
      // only them (the render below hides the update rows), and its answer
      // refuses every update exactly like this branch does.
      if (!globalSettings().updateNotificationsEnabled && precheck.droppedMods.length === 0) {
        await startLaunch(
          buildResolvedVersions(precheck.resolved, precheck.updates, new Set()),
          buildResolvedContent(precheck.resolvedContent, precheck.contentUpdates, new Set()),
        );
        return;
      }
      // An offline launch has no updates by construction (D115), so "nothing
      // to show" has to count the mods it will leave out too, or the popup
      // that asks about them would never open (D116).
      if (precheck.updates.length === 0 && shownContentUpdates.length === 0 && precheck.droppedMods.length === 0) {
        // Not `precheck.resolvedContent` as it stands: the rows of a category
        // that is switched off are updates nobody accepted, and handing over
        // their candidates would install them behind the checkbox's back.
        await startLaunch(
          precheck.resolved,
          buildResolvedContent(precheck.resolvedContent, precheck.contentUpdates, new Set()),
        );
        return;
      }
      // The **whole** payload is kept: the popup shows the categories that are
      // on, and the rows of the ones that are off still have to reach
      // `buildResolvedContent` to be refused there.
      setPendingUpdatePrecheck(precheck);
      return; // the popup decides, then launches
    }

    // Browser simulation (fallback when not in Tauri)
    setLaunchState("resolving");
    for (const stage of LAUNCH_STAGES) {
      setLaunchProgress(stage.progress);
      setLaunchStageLabel(stage.label);
      setLaunchStageDetail(stage.detail);
      setLaunchLogs(c => [...c, `[Resolver] ${stage.label} — ${stage.detail}`]);
      await wait(500);
    }
    setLaunchState("ready");
  };

  /**
   * The Play of a world card, and the "play the mod list" of its ⋮ menu.
   *
   * Both go through the ordinary launch — same pre-check, same popup, same
   * panel — after pointing the app at the mod list and the target the world
   * belongs to. The only difference is the world folder: with it the client
   * opens the world, without it the menu.
   *
   * It also switches to the mod list view, because the launch progress and
   * every question the launch can ask live there; leaving the user on the
   * home would look like nothing happened.
   */
  const handlePlayWorld = async (through: WorldInstanceLink, openTheWorld: boolean) => {
    if (launchState() === "resolving" || launchState() === "running" || updateCheckRunning()) return;

    const target = parseInstanceName(through.instanceName);
    if (!target) {
      pushUiError({
        title: "Unknown instance",
        message: `'${through.instanceName}' is not a target this launcher built.`,
        detail: "An instance directory is named '<minecraft version>-<loader>'.",
        severity: "warning",
        scope: "launch",
      });
      return;
    }

    // The mod list of the way in, not of the world: since D99 a shared world
    // belongs to none, and the instance chosen can be of any of them.
    await handleSelectModList(through.modlistName);
    // The instance the card was asked about: for a shared world (D95) the
    // home asked which one, and for an ordinary one there is only its own.
    setSelectedMcVersion(target.minecraftVersion);
    setSelectedModLoader(target.modLoader);
    setActiveRailView("modlist");
    // The folder name is that instance's own: a shared world can be linked
    // under a different name in each of them (D98), and Quick Play takes the
    // name the instance sees.
    await handleLaunch(openTheWorld ? through.folderName : null);
  };

  const handleOpenModlist = async (modlistName: string) => {
    setActiveRailView("modlist");
    await handleSelectModList(modlistName);
  };

  // ── Render ─────────────────────────────────────────────────────────────────
  return (
    <div class="flex flex-col h-screen w-screen overflow-hidden text-textMain bg-bgDark font-sans">
      {/* Header - Fixed at top */}
      <Header />
      <UpdateBanner />
      <NoticeBanner />

      {/* Main content area - Sidebar + Content + BottomBar */}
      <div class="flex flex-1 overflow-hidden relative">
        <Sidebar onSelectModList={handleSelectModList} />

        {/* One of four destinations: the home, a mod list, the skins, the gallery. */}
        <Switch>
          <Match when={activeRailView() === "home"}>
            <HomeView
              onPlayWorld={through => void handlePlayWorld(through, true)}
              onPlayModlistOf={through => void handlePlayWorld(through, false)}
              onOpenModlist={name => void handleOpenModlist(name)}
            />
          </Match>
          <Match when={activeRailView() === "skins"}>
            <SkinsScreen />
          </Match>
          <Match when={activeRailView() === "screenshots"}>
            <ScreenshotsView />
          </Match>
          <Match when={activeRailView() === "modlist"}>
            <div class="flex flex-col flex-1 min-w-0">
              <ModListEditor
                onAddMod={() => setAddModModalOpen(true)}
                onDeleteSelected={handleDeleteSelected}
                onReorder={orderedIds => void handleReorderRules(orderedIds)}
                onReorderAlts={(parentId, orderedIds) => void handleSaveAlternativeOrder(parentId, orderedIds)}
              />
              <LaunchPanel
                // The panel's own Play is the ordinary one: it must not
                // inherit the world a card asked for a moment ago.
                onLaunch={() => void handleLaunch()}
                onSwitchAccount={handleSwitchAccount}
                onVersionChange={handleVersionChange}
                onLoaderChange={handleLoaderChange}
              />
            </div>
          </Match>
        </Switch>
      </div>

      {/* Modals */}
      <AddModDialog onAddModrinth={handleAddModrinth} onAddContent={async (contentType, id, name) => {
        if (!selectedModListName()) return;
        try {
          await invoke("add_content_command", { input: { modlistName: selectedModListName(), contentType, id, source: "modrinth" } });
          seedContentName(id, name);
          bumpContentVersion();
        } catch (err) {
          pushUiError({ title: "Failed to add content", message: `Could not add '${id}'.`, detail: String(err), severity: "error", scope: "launch" });
        }
      }} onUploadLocal={handleUploadLocal} onDropLocal={handleDropLocal} />
      <CreateModlistModal onCreate={handleCreateModlist} />
      <SettingsModal onSave={handleSaveSettings} />
      <AccountsModal onSwitchAccount={handleSwitchAccount} />
      <FunctionalGroupModal />
      <LinkModal />
      <LinksOverviewModal />
      <AdvancedModPanel onDelete={id => { setSelectedIds([id]); void handleDeleteSelected(); }} />
      <PinVersionDialog />
      <InstancePresentationModal onSave={handleSavePresentation} onDelete={handleDeleteModList} />
      <IncompatibilitiesModal onSave={handleSaveIncompatibilities} />
      <AlternativesPanel onSave={handleSaveAlternativeOrder} onAddAlternative={handleAddAlternative} onRemoveAlternative={handleRemoveAlternative} />
      <ErrorCenter />
      <ExportModal onExport={handleExport} />
      <Show when={pendingUpdatePrecheck()} keyed>
        {precheck => {
          // D116: with update notifications off the popup only opens for the
          // mods the launch leaves out, and shows only those. No update row
          // means both accepted sets come back empty, which is the
          // notifications-off launch of `handleLaunch` (D30).
          const notifyUpdates = globalSettings().updateNotificationsEnabled;
          return (
            <UpdatePopup
              updates={notifyUpdates ? precheck.updates : []}
              contentUpdates={notifyUpdates ? precheck.contentUpdates.filter(row => contentCategoryNotificationsEnabled(row.category)) : []}
              contentWithoutVersions={notifyUpdates ? precheck.contentWithoutVersions : []}
              contentLookupFailures={notifyUpdates ? precheck.contentLookupFailures : []}
              versionNumberLookupError={notifyUpdates ? precheck.versionNumberLookupError : null}
              droppedMods={precheck.droppedMods}
              onChoose={(accepted, acceptedContent) => {
                setPendingUpdatePrecheck(null);
                void startLaunch(
                  buildResolvedVersions(precheck.resolved, precheck.updates, accepted),
                  // Every row, not only the ones on screen: a category whose
                  // checkbox is off was filtered out of `contentUpdates` before
                  // the popup, and those rows have to be refused, not dropped.
                  buildResolvedContent(precheck.resolvedContent, precheck.contentUpdates, acceptedContent),
                );
              }}
              // D26: the X and the backdrop cancel the launch. "Skip" means
              // "launch without updating"; closing means "I have not decided".
              // A cancelled launch also drops the world it was going to open,
              // which would otherwise sit there until the next Play took it.
              onCancel={() => { setPendingUpdatePrecheck(null); setQuickPlayWorldFolder(null); }}
            />
          );
        }}
      </Show>
    </div>
  );
}
