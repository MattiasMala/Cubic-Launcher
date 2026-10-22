import { batch } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { logger } from "../lib/logger";
import { MODRINTH_HEADERS } from "../lib/modrinth-user-agent";
import { normalizeModLoader, type AccountSummary, type ShellAccount, type FunctionalGroup, type LinkRule, type ModRow, type VersionRule, type CustomConfig } from "../lib/types";
import {
  modListCards, setModListCards, selectedModListName, setSelectedModListName,
  setModRowsState, setAccounts, setActiveAccountId,
  setGlobalSettings, setModlistOverrides,
  pushUiError,
  setAestheticGroups, setFunctionalGroups,
  setSavedIncompatibilities, setSavedLinks,
  setInstancePresentation,
  setModIcons, modIcons, setModNames, modNames,
  minecraftVersions, setSelectedMcVersion, setSelectedModLoader, selectedMcVersion, selectedModLoader,
  setVersionRules, setCustomConfigs,
  setResolvedModIds, setResolvedTarget, setExpandedRows,
} from "../store";
import { buildDefaultIconLabel, normalizeIdentifier, smartSetModRows } from "./row-state";

export const isTauri = () => typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

let resolutionSeq = 0;
export const modlistVersionLoaderCache = new Map<string, { version: string; loader: string }>();

/**
 * True once `minecraftVersions` holds the list the backend fetched, not the
 * placeholder of `store-state.ts`. Only then is its first entry a real "latest
 * release" that may be written as a mod list's version.
 */
let minecraftVersionsKnown = false;
export function markMinecraftVersionsKnown() {
  minecraftVersionsKnown = true;
}

/** The version proposed to a mod list that has none saved yet. */
export function defaultMinecraftVersion(): string {
  return minecraftVersions()[0] ?? "1.21.1";
}

/**
 * Write a mod list's Minecraft version and loader, and nothing else: the RAM,
 * JVM and wrapper overrides on disk stay as they are (todo.md:1618, audit 101
 * "Noto 1"). Rejects, so the caller can tell the user and undo.
 */
export async function saveModlistTarget(modlistName: string, version: string, loader: string) {
  await invoke("save_modlist_target_command", {
    input: { modlistName, minecraftVersion: version, modLoader: normalizeModLoader(loader) },
  });
}

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
  } catch (error) {
    logger.error("App", "loadAccounts failed", error);
    pushUiError({ title: "Could not load saved accounts", message: "The backend could not list the saved accounts.", detail: String(error), severity: "error", scope: "account" });
  }
}

export async function loadShellSnapshot(preferredName?: string | null) {
  try {
    const snap: any = await invoke("load_shell_snapshot_command", {
      selectedModlistName: preferredName ?? null,
    });

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
        author: typeof modlist.author === "string" && modlist.author.trim() ? modlist.author.trim() : undefined,
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

    setActiveAccountId(snap.active_account?.microsoft_id ?? "");
    await loadAccounts();

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
      // An override that is off shows what the launch would use without it,
      // never the value of the mod list shown before: ticking the box would
      // save that one on this list (audit 101, 085 scheda 7).
      setModlistOverrides({
        minRamEnabled: overrides.min_ram_mb != null,
        minRamMb: overrides.min_ram_mb ?? gs?.min_ram_mb ?? 2048,
        maxRamEnabled: overrides.max_ram_mb != null,
        maxRamMb: overrides.max_ram_mb ?? gs?.max_ram_mb ?? 4096,
        customArgsEnabled: overrides.custom_jvm_args != null,
        customJvmArgs: overrides.custom_jvm_args ?? "",
        profilerEnabled: overrides.profiler_enabled != null,
        profilerActive: overrides.profiler_enabled ?? false,
        wrapperEnabled: overrides.wrapper_command != null,
        wrapperCommand: overrides.wrapper_command ?? "",
      });
      const overridesName: string | null = overrides.modlist_name ?? null;
      const resolvedVersion: string = overrides.minecraft_version ?? defaultMinecraftVersion();
      const resolvedLoader = normalizeModLoader(overrides.mod_loader);
      setSelectedMcVersion(resolvedVersion);
      setSelectedModLoader(resolvedLoader);
      if (overridesName) {
        modlistVersionLoaderCache.set(overridesName, { version: resolvedVersion, loader: resolvedLoader });
        // A mod list with no saved version used to get the latest release at
        // every start, so a new Mojang release silently moved it to a new,
        // empty instance (audit 101, r2-primo-avvio-ambiente-degradato-1).
        // The default is written the first time it is shown, and from then on
        // it is the list's own. Not with the placeholder list: offline that
        // would pin "1.21.1" on a list that never asked for it.
        if ((overrides.minecraft_version == null || overrides.mod_loader == null) && minecraftVersionsKnown && isTauri()) {
          void saveModlistTarget(overridesName, resolvedVersion, resolvedLoader)
            .then(() => {
              logger.info("App", "saved the default target of a mod list that had none", { modlist: overridesName, version: resolvedVersion, loader: resolvedLoader });
              setModListCards(current => current.map(card => card.name === overridesName ? { ...card, mcVersion: resolvedVersion, modLoader: resolvedLoader } : card));
            })
            .catch(error => logger.warn("App", "could not save the default target of a mod list", { modlist: overridesName, error: String(error) }));
        }
      }
    }

    return snap;
  } catch (error) {
    logger.error("App", "loadShellSnapshot failed", error);
    pushUiError({ title: "Could not load launcher state", message: "The backend returned an error on startup.", detail: String(error), severity: "error", scope: "launch" });
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

    const incompat: Array<{ winnerId: string; loserId: string }> = snap.incompatibilities ?? [];

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

    // Rows and the states keyed by their ids change together: the persistence
    // effects must never see new rows with old ids (or the reverse), or they
    // would send a stale id to the backend (audit 101, modlist-5).
    batch(() => {
      setModRowsState(current => smartSetModRows(current, mappedRows));
      setSavedLinks(restoredLinks);
      if (resetGroups) setFunctionalGroups([]);
      setSavedIncompatibilities(incompat.map(rule => ({
        winnerId: primaryModToRowId.get(rule.winnerId) ?? rule.winnerId,
        loserId: primaryModToRowId.get(rule.loserId) ?? rule.loserId,
      })));
      setVersionRules(extractedVersionRules);
      setCustomConfigs(extractedCustomConfigs);
    });
    patchModNames();

    void invoke("backfill_availability_command", {
      modlistName,
      mcVersion: selectedMcVersion(),
      modLoader: normalizeModLoader(selectedModLoader()),
    }).catch(() => {}).then(() => {
      // The user may have moved to another mod list while the backfill ran:
      // resolving this one now would put its name on the indicators and throw
      // away the current list's resolution (audit 101, stato-frontend-8).
      if (selectedModListName() === modlistName) void runResolution(modlistName);
    });

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
