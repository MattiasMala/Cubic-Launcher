import { invoke } from "@tauri-apps/api/core";
import { logger } from "../lib/logger";
import { MODRINTH_HEADERS } from "../lib/modrinth-user-agent";
import { normalizeModLoader, type AccountSummary, type ShellAccount, type FunctionalGroup, type LinkRule, type ModRow, type VersionRule, type CustomConfig } from "../lib/types";
import {
  modListCards, setModListCards, selectedModListName, setSelectedModListName,
  setModRowsState, setAccounts, setActiveAccountId, setAccountsRevision, activePlaysOffline,
  setGlobalSettings, setModlistOverrides,
  pushUiError,
  setAestheticGroups, setFunctionalGroups,
  setSavedIncompatibilities, setSavedLinks,
  setInstancePresentation,
  setModIcons, modIcons, setModNames, modNames,
  minecraftVersions, setSelectedMcVersion, setSelectedModLoader, selectedMcVersion, selectedModLoader,
  setMinecraftVersions, setMcWithSnapshots, setMinecraftVersionsNotice,
  setVersionRules, setCustomConfigs,
  setResolvedModIds, setResolvedTarget, setExpandedRows,
} from "../store";
import { buildDefaultIconLabel, normalizeIdentifier, smartSetModRows } from "./row-state";

export const isTauri = () => typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

let resolutionSeq = 0;
export const modlistVersionLoaderCache = new Map<string, { version: string; loader: string }>();

export async function runResolution(modlistName?: string, mcVersion?: string, modLoader?: string) {
  const name = modlistName ?? selectedModListName();
  const version = mcVersion ?? selectedMcVersion();
  const loader = normalizeModLoader(modLoader ?? selectedModLoader());
  const seq = ++resolutionSeq;
  const target = name ? { modlistName: name, mcVersion: version, modLoader: loader } : null;
  setResolvedTarget(target);
  setResolvedModIds(null);
  if (!isTauri() || !name) return;
  try {
    const activeIds: string[] = await invoke("resolve_modlist_command", {
      modlistName: name,
      mcVersion: version,
      modLoader: loader,
    });
    if (seq !== resolutionSeq) return;
    setResolvedTarget(target);
    setResolvedModIds(new Set(activeIds));
  } catch (error) {
    if (seq !== resolutionSeq) return;
    logger.warn("App", "resolution failed", { error });
    setResolvedTarget(target);
    setResolvedModIds(null);
  }
}

/** `MinecraftVersionsPayload` in minecraft_versions.rs. */
type MinecraftVersionsPayload = {
  releases: string[];
  withSnapshots: string[];
  /** `mojang`: fetched now; `saved`: the manifest saved last time; `installed`: only the versions on disk. */
  source: "mojang" | "saved" | "installed";
  playsOffline: boolean;
};

function minecraftVersionsNoticeFor(payload: MinecraftVersionsPayload): string | null {
  if (payload.source === "mojang") return null;
  const why = payload.playsOffline
    ? "This account plays offline, so Mojang wasn't asked"
    : "Mojang couldn't be reached";
  const what = payload.source === "saved"
    ? "the list is the one saved last time, plus the versions already downloaded. Newer versions may be missing."
    : "only the versions already downloaded are listed.";
  const fallback = payload.releases.length === 0
    ? " No version is downloaded yet, so a few common versions are shown instead."
    : "";
  return `${why}: ${what}${fallback}`;
}

/**
 * The version dropdown's lists. Never awaited by the startup (stato-frontend-7):
 * the backend has a 20 s timeout, falls back to what is on disk, and asks
 * nobody while the active account plays offline (D140); the notice says when
 * the list may be partial (r2-primo-avvio-ambiente-degradato-2).
 */
export async function loadMinecraftVersions() {
  if (!isTauri()) return;
  try {
    const payload = await invoke<MinecraftVersionsPayload>("fetch_minecraft_versions_command");
    logger.debug("App", "fetched minecraft versions", {
      releases: payload.releases.length,
      withSnapshots: payload.withSnapshots.length,
      source: payload.source,
    });
    if (payload.releases.length > 0) setMinecraftVersions(payload.releases);
    if (payload.withSnapshots.length > 0) setMcWithSnapshots(payload.withSnapshots);
    setMinecraftVersionsNotice(minecraftVersionsNoticeFor(payload));
  } catch (error) {
    logger.warn("App", "failed to fetch minecraft versions, using defaults", error);
    setMinecraftVersionsNotice("The version list couldn't be loaded, so a few common versions are shown instead.");
  }
}

function toAccountSummary(account: ShellAccount): AccountSummary {
  return {
    id: account.microsoft_id,
    gamertag: account.xbox_gamertag?.trim() || account.microsoft_id,
    email: account.microsoft_id,
    avatarUrl: account.avatar_url ?? undefined,
    status: account.status,
    lastMode: account.last_mode,
    credentials: account.credentials,
    credentialsDetail: account.credentials_detail,
    playsOffline: account.plays_offline,
  };
}

/**
 * The saved accounts as the backend lists them (D113): every row in the
 * database, the active one first, and those whose sign-in can't be read too
 * (D117). The list replaces what the session had instead of being merged
 * with it; status and credentials are the backend's word, never assumed (F1).
 */
export async function loadAccounts() {
  try {
    const list = await invoke<ShellAccount[]>("list_accounts_command");
    setAccounts(list.map(toAccountSummary));
    // The active account is the backend's word too (todo.md:1390): after a
    // removal the backend may have activated another row.
    setActiveAccountId(list.find(account => account.is_active)?.microsoft_id ?? "");
    setAccountsRevision(revision => revision + 1);
  } catch (error) {
    logger.error("App", "loadAccounts failed", error);
    pushUiError({ title: "Could not load saved accounts", message: "The backend could not list the saved accounts.", detail: String(error), severity: "error", scope: "account" });
  }
}

/** Folder → error of the unreadable mod lists already reported, so that every reload doesn't repeat them. */
const reportedUnreadableModlists = new Map<string, string>();

function reportUnreadableModlists(unreadable: Array<{ folder: string; error: string }>) {
  const current = new Set(unreadable.map(entry => entry.folder));
  for (const folder of [...reportedUnreadableModlists.keys()]) {
    if (!current.has(folder)) reportedUnreadableModlists.delete(folder);
  }
  for (const { folder, error } of unreadable) {
    if (reportedUnreadableModlists.get(folder) === error) continue;
    reportedUnreadableModlists.set(folder, error);
    pushUiError({
      title: "Mod list skipped",
      message: `The rules.json of '${folder}' can't be read, so that mod list is not shown. The other mod lists are not affected.`,
      detail: error,
      severity: "warning",
      scope: "launch",
    });
  }
}

export async function loadShellSnapshot(preferredName?: string | null) {
  // The accounts are a call of their own (todo.md:1414): a snapshot that
  // fails must not leave the launcher without its account list.
  const accountsLoaded = loadAccounts();
  try {
    const snap: any = await invoke("load_shell_snapshot_command", {
      selectedModlistName: preferredName ?? null,
    });
    reportUnreadableModlists(snap.unreadable_modlists ?? []);

    const existing = modListCards();
    const cards = (snap.modlists ?? []).map((modlist: any) => {
      const prev = existing.find(card => card.name === modlist.name);
      return {
        name: modlist.name,
        status: "Ready" as const,
        accent: "from-primary/30 via-primary/10 to-transparent",
        description: modlist.description || (modlist.rule_count > 0 ? `${modlist.rule_count} rules` : "Empty mod list"),
        iconImage: prev?.iconImage,
        iconLabel: prev?.iconLabel,
        iconAccent: prev?.iconAccent,
        displayName: prev?.displayName,
        mcVersion: modlist.minecraft_version ?? undefined,
        modLoader: normalizeModLoader(modlist.mod_loader),
      };
    });
    setModListCards(cards);

    const needLoad = cards.filter((card: any) => !card.iconLabel).map((card: any) => card.name);
    if (needLoad.length > 0) void loadAllCardIcons(needLoad);

    if (cards.length > 0) {
      const selected = preferredName && cards.some((card: any) => card.name === preferredName)
        ? preferredName
        : cards[0].name;
      setSelectedModListName(selected);
    } else {
      setSelectedModListName("");
    }

    await accountsLoaded;

    const gs = snap.global_settings;
    if (gs) {
      setGlobalSettings({
        minRamMb: gs.min_ram_mb ?? 2048,
        maxRamMb: gs.max_ram_mb ?? 4096,
        customJvmArgs: gs.custom_jvm_args ?? "",
        profilerEnabled: gs.profiler_enabled ?? false,
        // The defaults match the backend's (`app_shell.rs`): a key the
        // database does not have yet means notifications on, every category
        // included.
        updateNotificationsEnabled: gs.update_notifications_enabled ?? true,
        updateNotificationsResourcePacks: gs.update_notifications_resource_packs ?? true,
        updateNotificationsDataPacks: gs.update_notifications_data_packs ?? true,
        updateNotificationsShaders: gs.update_notifications_shaders ?? true,
        wrapperCommand: gs.wrapper_command ?? "",
        javaPathOverride: gs.java_path_override ?? "",
      });
    }

    const overrides = snap.selected_modlist_overrides;
    if (overrides) {
      setModlistOverrides(current => ({
        ...current,
        minRamEnabled: overrides.min_ram_mb != null,
        minRamMb: overrides.min_ram_mb ?? current.minRamMb,
        maxRamEnabled: overrides.max_ram_mb != null,
        maxRamMb: overrides.max_ram_mb ?? current.maxRamMb,
        customArgsEnabled: overrides.custom_jvm_args != null,
        customJvmArgs: overrides.custom_jvm_args ?? current.customJvmArgs,
        profilerEnabled: overrides.profiler_enabled != null,
        profilerActive: overrides.profiler_enabled ?? current.profilerActive,
        wrapperEnabled: overrides.wrapper_command != null,
        wrapperCommand: overrides.wrapper_command ?? current.wrapperCommand,
      }));
      const resolvedVersion = overrides.minecraft_version ?? minecraftVersions()[0] ?? "1.21.1";
      const resolvedLoader = normalizeModLoader(overrides.mod_loader);
      setSelectedMcVersion(resolvedVersion);
      setSelectedModLoader(resolvedLoader);
      if (preferredName) modlistVersionLoaderCache.set(preferredName, { version: resolvedVersion, loader: resolvedLoader });
    }

    return snap;
  } catch (error) {
    logger.error("App", "loadShellSnapshot failed", error);
    pushUiError({ title: "Could not load launcher state", message: "The backend returned an error on startup.", detail: String(error), severity: "error", scope: "launch" });
    await accountsLoaded;
    return null;
  }
}

function mapEditorRow(row: any, ruleIndex: number, path: string): ModRow {
  const nameId = normalizeIdentifier(row.name);
  const id = `rule-${ruleIndex}${path}-${nameId}`;
  const alternatives = (row.alternatives ?? []).map((alt: any, altIdx: number) =>
    mapEditorRow(alt, ruleIndex, `${path}-alternative-${altIdx + 1}`)
  );
  return {
    id,
    name: row.name as string,
    modrinth_id: row.source === "modrinth" ? (row.modId as string) : undefined,
    iconImage: (row.iconImage as string | null | undefined) ?? undefined,
    primaryModId: row.modId as string,
    kind: row.source as "modrinth" | "local",
    enabled: row.enabled !== false,
    area: "Rule",
    note: alternatives.length > 0 ? `Primary option with ${alternatives.length + 1} mods.` : "Primary option with 1 mod.",
    tags: [],
    alternatives,
    links: (row.requires ?? []) as string[],
    altGroups: [],
    pin: row.pin ? { ownsJar: row.pin.ownsJar === true } : undefined,
  };
}

function mapEditorRowsToModRows(rows: any[]): ModRow[] {
  return rows.map((row: any, index: number) => mapEditorRow(row, index, ""));
}

export async function loadEditorSnapshot(modlistName: string, resetGroups = false) {
  if (!modlistName) return null;
  try {
    const snap: any = await invoke("load_modlist_editor_command", { selectedModlistName: modlistName });
    const mappedRows = mapEditorRowsToModRows(snap.rows ?? []);

    setModRowsState(current => smartSetModRows(current, mappedRows));
    patchModNames();

    const primaryModToRowId = new Map<string, string>();
    const collectPrimaryMods = (rows: ModRow[]) => {
      for (const row of rows) {
        if (row.primaryModId) primaryModToRowId.set(row.primaryModId, row.id);
        if (row.alternatives?.length) collectPrimaryMods(row.alternatives);
      }
    };
    collectPrimaryMods(mappedRows);

    const restoredLinks: LinkRule[] = [];
    const collectLinks = (rows: ModRow[]) => {
      for (const row of rows) {
        for (const primaryModId of (row.links ?? []) as string[]) {
          const toId = primaryModToRowId.get(primaryModId);
          if (toId) restoredLinks.push({ fromId: row.id, toId });
        }
        if (row.alternatives?.length) collectLinks(row.alternatives);
      }
    };
    collectLinks(mappedRows);
    setSavedLinks(restoredLinks);

    if (resetGroups) {
      setFunctionalGroups([]);
    }

    const incompat: Array<{ winnerId: string; loserId: string }> = snap.incompatibilities ?? [];
    setSavedIncompatibilities(incompat.map(rule => ({
      winnerId: primaryModToRowId.get(rule.winnerId) ?? rule.winnerId,
      loserId: primaryModToRowId.get(rule.loserId) ?? rule.loserId,
    })));

    const extractedVersionRules: VersionRule[] = [];
    const extractedCustomConfigs: CustomConfig[] = [];
    const collectAdvanced = (rows: any[]) => {
      for (const row of rows) {
        const backendModId = row.modId as string;
        const rowId = primaryModToRowId.get(backendModId) ?? backendModId;
        for (const [index, rule] of (row.versionRules ?? []).entries()) {
          extractedVersionRules.push({ id: `vr-${backendModId}-${index}`, modId: rowId, kind: rule.kind as "exclude" | "only", mcVersions: rule.mcVersions ?? [], loader: rule.loader ?? "any" });
        }
        for (const config of row.customConfigs ?? []) {
          extractedCustomConfigs.push({ id: config.id, modId: rowId, mcVersions: config.mcVersions ?? [], loader: config.loader ?? "any", targetPath: config.targetPath ?? "", files: config.files ?? [] });
        }
        if (row.alternatives?.length) collectAdvanced(row.alternatives);
      }
    };
    collectAdvanced(snap.rows ?? []);
    setVersionRules(extractedVersionRules);
    setCustomConfigs(extractedCustomConfigs);

    void invoke("backfill_availability_command", {
      modlistName,
      mcVersion: selectedMcVersion(),
      modLoader: normalizeModLoader(selectedModLoader()),
    }).catch(() => {}).then(() => runResolution(modlistName));

    return snap;
  } catch (error) {
    logger.error("App", "loadEditorSnapshot failed", error);
    pushUiError({ title: "Could not load mod list", message: `Failed to read rules for '${modlistName}'.`, detail: String(error), severity: "error", scope: "launch" });
    setModRowsState([]);
    return null;
  }
}

export async function loadAllCardIcons(names: string[]) {
  if (!isTauri() || names.length === 0) {
    logger.warn("App", "loadAllCardIcons skipped - no backend");
    return;
  }
  await Promise.all(names.map(async name => {
    try {
      const presentation: any = await invoke("load_modlist_presentation_command", { modlistName: name });
      const iconImage: string = presentation.iconImage ?? "";
      const iconLabel: string = presentation.iconLabel ?? "";
      const iconAccent: string = presentation.iconAccent ?? "";
      const displayName: string = presentation.displayName ?? name;
      setModListCards(cards => cards.map(card => card.name === name ? { ...card, iconImage, iconLabel, iconAccent, displayName } : card));
    } catch (_) {
      // silently skip
    }
  }));
}

export async function loadModlistPresentation(modlistName: string) {
  if (!modlistName) {
    setInstancePresentation({ displayName: "", iconLabel: "ML", iconAccent: "", notes: "", iconImage: "" });
    return null;
  }

  if (!isTauri()) {
    logger.warn("App", "loadModlistPresentation skipped - no backend");
    setInstancePresentation({ displayName: modlistName, iconLabel: buildDefaultIconLabel(modlistName), iconAccent: "", notes: "", iconImage: "" });
    return null;
  }

  try {
    const presentation: any = await invoke("load_modlist_presentation_command", { modlistName });
    const iconImage = presentation.iconImage ?? "";
    const iconLabel = presentation.iconLabel ?? buildDefaultIconLabel(modlistName);
    const iconAccent = presentation.iconAccent ?? "";
    const displayName = presentation.displayName ?? modlistName;
    setInstancePresentation({
      displayName,
      iconLabel,
      iconAccent,
      notes: presentation.notes ?? "",
      iconImage,
    });
    setModListCards(cards => cards.map(card => card.name === modlistName ? { ...card, iconImage, iconLabel, iconAccent, displayName } : card));
    return presentation;
  } catch (error) {
    setInstancePresentation({ displayName: modlistName, iconLabel: buildDefaultIconLabel(modlistName), iconAccent: "", notes: "", iconImage: "" });
    pushUiError({ title: "Could not load notes", message: `Saved notes for '${modlistName}' could not be loaded.`, detail: String(error), severity: "error", scope: "launch" });
    return null;
  }
}

export async function loadModlistGroups(modlistName: string, rows: ModRow[]) {
  if (!modlistName) {
    setFunctionalGroups([]);
    return null;
  }

  if (!isTauri()) {
    logger.warn("App", "loadModlistGroups skipped - no backend");
    setFunctionalGroups([]);
    return null;
  }

  try {
    const layout: any = await invoke("load_modlist_groups_command", { modlistName });
    const availableIds = new Set<string>();
    const modIdToRowId = new Map<string, string>();
    const collect = (items: ModRow[]) => {
      for (const row of items) {
        availableIds.add(row.id);
        if (row.primaryModId) modIdToRowId.set(row.primaryModId, row.id);
        if (row.alternatives?.length) collect(row.alternatives);
      }
    };
    collect(rows);

    const resolveId = (id: string): string | null => {
      if (availableIds.has(id)) return id;
      const mapped = modIdToRowId.get(id);
      return mapped && availableIds.has(mapped) ? mapped : null;
    };

    const tagDefs = layout.tags ?? layout.functionalGroups ?? [];
    const nextFunctionalGroups: FunctionalGroup[] = tagDefs.map((group: any) => ({
      id: group.id,
      name: group.name,
      tone: group.tone,
      modIds: (group.modIds ?? group.mod_ids ?? []).map((id: string) => resolveId(id)).filter((id: string | null): id is string => id !== null),
    }));
    setFunctionalGroups(nextFunctionalGroups.filter(group => group.modIds.length > 0));

    const persistedAesthetic: any[] = layout.aestheticGroups ?? [];
    setAestheticGroups(persistedAesthetic.map((group: any) => ({
      id: group.id as string,
      name: group.name as string,
      collapsed: group.collapsed as boolean,
      blockIds: (group.blockIds ?? []).map((id: string) => resolveId(id)).filter((id: string | null): id is string => id !== null),
      scopeRowId: (() => {
        const scopeId = group.scopeRowId as string | null;
        return scopeId ? (resolveId(scopeId) ?? null) : null;
      })(),
    })));

    const persistedExpanded: string[] = (layout.collapsedAlts ?? []).map((id: string) => resolveId(id)).filter((id: string | null): id is string => id !== null);
    setExpandedRows(persistedExpanded);

    return layout;
  } catch (error) {
    setFunctionalGroups([]);
    pushUiError({ title: "Could not load groups", message: `Saved groups for '${modlistName}' could not be loaded.`, detail: String(error), severity: "error", scope: "launch" });
    return null;
  }
}

function collectModrinthIds(rows: ModRow[]): Set<string> {
  const ids = new Set<string>();
  const collect = (list: ModRow[]) => {
    for (const row of list) {
      if (row.modrinth_id) ids.add(row.modrinth_id);
      if (row.alternatives) collect(row.alternatives);
    }
  };
  collect(rows);
  return ids;
}

export async function fetchModMetadata(rows: ModRow[]) {
  const ids = [...collectModrinthIds(rows)];
  if (ids.length === 0) return;
  // Only this entry point holds `ModRow`s, so only this one patches their
  // names: before, for names already cached, and after the request.
  patchModNames();
  await fetchMetadataForIds(ids);
  patchModNames();
}

/**
 * Fill `modIcons` and `modNames` for these Modrinth project ids (or slugs) with
 * **one** request for the ones not already known.
 *
 * Id-driven because not every caller holds `ModRow`s: the update popup's rows
 * name a `projectId` and nothing else. It writes the two signals and nothing
 * else — no `modRowsState` rewrite on a path that has nothing to do with the
 * mod list — and whoever needs the rows patched does it itself.
 */
export async function fetchMetadataForIds(ids: string[]) {
  // D140: with the active account playing offline nothing is asked; the rows
  // keep the names already known this session, else their ids.
  if (activePlaysOffline()) return;
  const missing = ids.filter(id => !modIcons().has(id));
  if (missing.length === 0) return;

  try {
    const param = encodeURIComponent(JSON.stringify(missing));
    const response = await fetch(
      `https://api.modrinth.com/v2/projects?ids=${param}`,
      { headers: MODRINTH_HEADERS }
    );
    if (!response.ok) return;
    const projects: Array<{ id: string; slug: string; title: string; icon_url?: string | null }> = await response.json();
    const updatedIcons = new Map(modIcons());
    const updatedNames = new Map(modNames());
    for (const project of projects) {
      if (project.icon_url) {
        if (project.id) updatedIcons.set(project.id, project.icon_url);
        if (project.slug) updatedIcons.set(project.slug, project.icon_url);
      }
      if (project.title) {
        if (project.id) updatedNames.set(project.id, project.title);
        if (project.slug) updatedNames.set(project.slug, project.title);
      }
    }
    setModIcons(updatedIcons);
    setModNames(updatedNames);
  } catch {
      // metadata is best-effort
  }
}

function patchModNames() {
  if (modNames().size === 0) return;
  setModRowsState(current => current.map(function patch(row: ModRow): ModRow {
    const realName = row.modrinth_id ? modNames().get(row.modrinth_id) : undefined;
    const needsPatch = realName && (row.name === row.modrinth_id || row.name === row.primaryModId);
    const nameFixed = needsPatch ? { ...row, name: realName } : row;
    if (nameFixed.alternatives?.length) {
      const patchedAlternatives = nameFixed.alternatives.map(patch);
      return { ...nameFixed, alternatives: patchedAlternatives };
    }
    return nameFixed;
  }));
}

/**
 * Seed the readable-name cache from a name the UI already knows (e.g. the
 * add-mod dialog result) so cards never regress to showing the raw Modrinth
 * project ID when the metadata fetch is unavailable.
 */
export function seedModName(id: string, name: string) {
  if (!id || !name || name === id) return;
  setModNames(current => new Map(current).set(id, name));
  patchModNames();
}
