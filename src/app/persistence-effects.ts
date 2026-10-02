import { createEffect, createSignal } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import {
  aestheticGroups,
  customConfigs,
  expandedRows,
  functionalGroups,
  pushUiError,
  rowMap,
  savedIncompatibilities,
  savedLinks,
  selectedModListName,
  versionRules,
} from "../store";
import { isTauri, runResolution } from "./backend-loaders";
import {
  serializeExpandedRows,
  serializeGroupsLayout,
  serializeIncompatibilities,
  serializeLinks,
  serializeRuleGroups,
} from "./row-state";

type IncompatibilityRule = {
  winnerId: string;
  loserId: string;
};

export function useAppPersistence() {
  const [groupLayoutReady, setGroupLayoutReady] = createSignal(false);
  const [lastSavedGroupLayout, setLastSavedGroupLayout] = createSignal("");
  const [lastSavedRuleMetaGroups, setLastSavedRuleMetaGroups] = createSignal("");
  const [lastSavedCollapsedAlts, setLastSavedCollapsedAlts] = createSignal("");
  const [incompatReady, setIncompatReady] = createSignal(false);
  const [lastSavedIncompat, setLastSavedIncompat] = createSignal("");
  const [linksReady, setLinksReady] = createSignal(false);
  const [, setLastSavedLinks] = createSignal("");
  const [advancedReady, setAdvancedReady] = createSignal(false);
  const [lastSavedAdvanced, setLastSavedAdvanced] = createSignal("");

  const rowIdToModId = (id: string): string => rowMap().get(id)?.primaryModId ?? id;

  const buildGroupsPayload = (modlistName: string) => ({
    modlistName,
    tags: functionalGroups().map(group => ({
      id: group.id,
      name: group.name,
      tone: group.tone,
      modIds: group.modIds.map(rowIdToModId),
    })),
    aestheticGroups: aestheticGroups().map(group => ({
      id: group.id,
      name: group.name,
      collapsed: group.collapsed,
      blockIds: group.blockIds.map(rowIdToModId),
      scopeRowId: group.scopeRowId ? rowIdToModId(group.scopeRowId) : null,
    })),
    collapsedAlts: [...expandedRows()].map(rowIdToModId),
  });

  createEffect(() => {
    const modlistName = selectedModListName();
    const serialized = serializeGroupsLayout(functionalGroups());

    if (!groupLayoutReady() || !modlistName || !isTauri() || serialized === lastSavedGroupLayout()) return;

    setLastSavedGroupLayout(serialized);
    void invoke("save_modlist_groups_command", { input: buildGroupsPayload(modlistName) }).catch(error => {
      setLastSavedGroupLayout("");
      pushUiError({ title: "Could not save groups", message: "The mod-list group layout could not be persisted.", detail: String(error), severity: "error", scope: "launch" });
    });
  });

  createEffect(() => {
    const modlistName = selectedModListName();
    const serialized = serializeRuleGroups(aestheticGroups());

    if (!groupLayoutReady() || !modlistName || !isTauri() || serialized === lastSavedRuleMetaGroups()) return;

    setLastSavedRuleMetaGroups(serialized);
    void invoke("save_modlist_groups_command", { input: buildGroupsPayload(modlistName) }).catch(error => {
      setLastSavedRuleMetaGroups("");
      pushUiError({ title: "Could not save groups", message: "The aesthetic group layout could not be persisted.", detail: String(error), severity: "error", scope: "launch" });
    });
  });

  createEffect(() => {
    const modlistName = selectedModListName();
    const serialized = serializeExpandedRows(expandedRows());

    if (!groupLayoutReady() || !modlistName || !isTauri() || serialized === lastSavedCollapsedAlts()) return;

    setLastSavedCollapsedAlts(serialized);
    void invoke("save_modlist_groups_command", { input: buildGroupsPayload(modlistName) }).catch(error => {
      setLastSavedCollapsedAlts("");
      pushUiError({ title: "Could not save groups", message: "The alt panel state could not be persisted.", detail: String(error), severity: "error", scope: "launch" });
    });
  });

  createEffect(() => {
    const modlistName = selectedModListName();
    const serialized = serializeIncompatibilities(savedIncompatibilities());

    if (!incompatReady() || !modlistName || !isTauri() || serialized === lastSavedIncompat()) return;

    setLastSavedIncompat(serialized);
    const rules = savedIncompatibilities().map(rule => ({
      winnerId: rowMap().get(rule.winnerId)?.primaryModId ?? rule.winnerId,
      loserId: rowMap().get(rule.loserId)?.primaryModId ?? rule.loserId,
    }));
    void invoke("save_incompatibilities_command", {
      input: { modlistName, rules },
    }).then(() => runResolution()).catch(error => {
      setLastSavedIncompat("");
      pushUiError({ title: "Could not save incompatibilities", message: "The incompatibility rules could not be persisted.", detail: String(error), severity: "error", scope: "launch" });
    });
  });

  createEffect(() => {
    const modlistName = selectedModListName();
    const links = savedLinks();
    const vr = versionRules();
    const cc = customConfigs();
    const serialized = JSON.stringify({ links: serializeLinks(links), vr, cc });

    if (!linksReady() || !advancedReady() || !modlistName || !isTauri() || serialized === lastSavedAdvanced()) return;

    setLastSavedAdvanced(serialized);
    setLastSavedLinks(serializeLinks(links));

    const requiresByModId = new Map<string, string[]>();
    for (const link of links) {
      const fromModId = rowMap().get(link.fromId)?.primaryModId ?? link.fromId;
      const toModId = rowMap().get(link.toId)?.primaryModId ?? link.toId;
      const requires = requiresByModId.get(fromModId) ?? [];
      requires.push(toModId);
      requiresByModId.set(fromModId, requires);
    }
    const requiresEntries = [...requiresByModId.entries()].map(([modId, requires]) => ({ modId, requires }));

    const vrByMod = new Map<string, typeof vr>();
    for (const rule of vr) {
      const modId = rowMap().get(rule.modId)?.primaryModId ?? rule.modId;
      const rules = vrByMod.get(modId) ?? [];
      rules.push(rule);
      vrByMod.set(modId, rules);
    }
    const versionRulesEntries = [...vrByMod.entries()].map(([modId, rules]) => ({
      modId,
      versionRules: rules.map(rule => ({ kind: rule.kind, mcVersions: rule.mcVersions, loader: rule.loader })),
    }));

    const ccByMod = new Map<string, typeof cc>();
    for (const config of cc) {
      const modId = rowMap().get(config.modId)?.primaryModId ?? config.modId;
      const configs = ccByMod.get(modId) ?? [];
      configs.push(config);
      ccByMod.set(modId, configs);
    }
    const customConfigsEntries = [...ccByMod.entries()].map(([modId, configs]) => ({
      modId,
      customConfigs: configs.map(config => ({
        id: config.id,
        mcVersions: config.mcVersions,
        loader: config.loader,
        targetPath: config.targetPath,
        files: config.files,
      })),
    }));

    void invoke("save_advanced_batch_command", {
      input: { modlistName, requiresEntries, versionRulesEntries, customConfigsEntries },
    }).then(() => runResolution()).catch(error => {
      setLastSavedAdvanced("");
      pushUiError({ title: "Could not save rule config", message: "Failed to persist advanced config.", detail: String(error), severity: "error", scope: "launch" });
    });
  });

  const syncPersistedState = () => {
    setLastSavedGroupLayout(serializeGroupsLayout(functionalGroups()));
    setLastSavedRuleMetaGroups(serializeRuleGroups(aestheticGroups()));
    setLastSavedCollapsedAlts(serializeExpandedRows(expandedRows()));
    setLastSavedIncompat(serializeIncompatibilities(savedIncompatibilities()));
    setLastSavedLinks(serializeLinks(savedLinks()));
    setLastSavedAdvanced(JSON.stringify({ links: serializeLinks(savedLinks()), vr: versionRules(), cc: customConfigs() }));
  };

  const primePersistenceState = () => {
    syncPersistedState();
    setGroupLayoutReady(true);
    setIncompatReady(true);
    setLinksReady(true);
    setAdvancedReady(true);
  };

  const resetPersistenceState = () => {
    setGroupLayoutReady(false);
    setIncompatReady(false);
    setLinksReady(false);
    setAdvancedReady(false);
  };

  const markIncompatibilitiesSaved = (rules: IncompatibilityRule[]) => {
    setLastSavedIncompat(serializeIncompatibilities(rules));
  };

  return {
    markIncompatibilitiesSaved,
    primePersistenceState,
    resetPersistenceState,
  };
}
