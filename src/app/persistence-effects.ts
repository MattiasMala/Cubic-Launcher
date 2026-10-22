import { createEffect, createSignal, untrack, type Accessor, type Setter } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { logger } from "../lib/logger";
import {
  aestheticGroups,
  customConfigs,
  expandedRows,
  functionalGroups,
  modRowsState,
  pushUiError,
  rowMap,
  savedIncompatibilities,
  savedLinks,
  selectedModListName,
  versionRules,
} from "../store";
import type { CustomConfig, LinkRule, VersionRule } from "../lib/types";
import { isTauri, loadEditorSnapshot, runResolution } from "./backend-loaders";
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

/**
 * Attese prima di ritentare un salvataggio fallito; finite le voci, si smette
 * e si aspetta la prossima modifica.
 */
const SAVE_RETRY_DELAYS_MS = [2_000, 10_000];

/**
 * Un salvataggio fallito non rimette subito `""` nel suo «ultimo salvato»:
 * l'effetto rileggerebbe lo stesso stato e lo rimanderebbe all'istante, senza
 * fine — notifiche a raffica e CPU alta per un batch che il backend rifiuta
 * sempre (audit 101, `stato-frontend-4`). Si ritenta poche volte con attese
 * crescenti, l'errore si mostra una volta per stato fallito, e poi resta la
 * prossima modifica dell'utente, che rimanda lo stato intero.
 */
function createSaveRetry(label: string, lastSaved: Accessor<string>, setLastSaved: Setter<string>) {
  let attempts = 0;
  let retryingState: string | null = null;
  let timer: ReturnType<typeof setTimeout> | undefined;

  return {
    succeeded() {
      attempts = 0;
      retryingState = null;
    },
    failed(serialized: string, notify: () => void) {
      if (serialized !== retryingState) {
        attempts = 0;
        notify();
      }
      if (attempts >= SAVE_RETRY_DELAYS_MS.length) {
        logger.warn("App", `${label}: save still failing, waiting for the next change`);
        attempts = 0;
        retryingState = null;
        return;
      }
      const delay = SAVE_RETRY_DELAYS_MS[attempts++];
      retryingState = serialized;
      clearTimeout(timer);
      timer = setTimeout(() => {
        // Solo se nel frattempo non è cambiato niente: un'altra modifica ha già
        // fatto partire il suo salvataggio, un cambio di mod list ha riallineato.
        if (lastSaved() === serialized) setLastSaved("");
      }, delay);
    },
    cancel() {
      clearTimeout(timer);
      attempts = 0;
      retryingState = null;
    },
  };
}

/**
 * Il batch avanzato in termini di mod id, cioè quello che va al backend, e le
 * righe che le voci nominano ma che `rowMap` non ha più.
 *
 * Una voce la cui riga non c'è più non manda l'id di riga come mod id: il
 * backend rifiuterebbe l'intero batch con «unknown rule id in advanced batch»
 * (audit 101, `modlist-5`). Ma il backend tratta il batch come lo stato intero
 * — svuota requires, version rule e custom config di ogni regola e riscrive
 * solo quello che arriva — quindi un batch senza quelle voci le cancellerebbe
 * dal disco: chi lo riceve con `skipped` non vuoto non lo manda
 * (review-frontend-stato-19). Succede solo in un intervallo (la cancellazione
 * ottimistica prima che il backend risponda, un editor che non si è caricato),
 * e il batch giusto parte quando link, version rule e config tornano allineati
 * alle righe. `rowMap` è letto senza tracciarlo: l'effetto riparte quando
 * cambiano link, version rule o custom config, non a ogni modifica delle righe.
 */
function buildAdvancedPayload(links: LinkRule[], vr: VersionRule[], cc: CustomConfig[]) {
  const rows = untrack(rowMap);
  const skipped = new Set<string>();
  const modIdOf = (rowId: string): string | null => {
    const modId = rows.get(rowId)?.primaryModId;
    if (!modId) skipped.add(rowId);
    return modId ?? null;
  };

  const requiresByModId = new Map<string, string[]>();
  for (const link of links) {
    const fromModId = modIdOf(link.fromId);
    const toModId = modIdOf(link.toId);
    if (!fromModId || !toModId) continue;
    const requires = requiresByModId.get(fromModId) ?? [];
    requires.push(toModId);
    requiresByModId.set(fromModId, requires);
  }
  const requiresEntries = [...requiresByModId.entries()].map(([modId, requires]) => ({ modId, requires }));

  const vrByMod = new Map<string, VersionRule[]>();
  for (const rule of vr) {
    const modId = modIdOf(rule.modId);
    if (!modId) continue;
    const rules = vrByMod.get(modId) ?? [];
    rules.push(rule);
    vrByMod.set(modId, rules);
  }
  const versionRulesEntries = [...vrByMod.entries()].map(([modId, rules]) => ({
    modId,
    versionRules: rules.map(rule => ({ kind: rule.kind, mcVersions: rule.mcVersions, loader: rule.loader })),
  }));

  const ccByMod = new Map<string, CustomConfig[]>();
  for (const config of cc) {
    const modId = modIdOf(config.modId);
    if (!modId) continue;
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

  return { payload: { requiresEntries, versionRulesEntries, customConfigsEntries }, skipped };
}

/** Some row has the shape of a pin made before the marker: local, one Modrinth alternative. */
function mayBeLegacyPin(): boolean {
  return modRowsState().some(row =>
    row.kind === "local" && row.alternatives?.length === 1 && row.alternatives[0].kind === "modrinth");
}

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

  const groupLayoutRetry = createSaveRetry("groups", lastSavedGroupLayout, setLastSavedGroupLayout);
  const ruleMetaGroupsRetry = createSaveRetry("aesthetic groups", lastSavedRuleMetaGroups, setLastSavedRuleMetaGroups);
  const collapsedAltsRetry = createSaveRetry("alt panel state", lastSavedCollapsedAlts, setLastSavedCollapsedAlts);
  const incompatRetry = createSaveRetry("incompatibilities", lastSavedIncompat, setLastSavedIncompat);
  const advancedRetry = createSaveRetry("rule config", lastSavedAdvanced, setLastSavedAdvanced);
  const allRetries = [groupLayoutRetry, ruleMetaGroupsRetry, collapsedAltsRetry, incompatRetry, advancedRetry];

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
    void invoke("save_modlist_groups_command", { input: buildGroupsPayload(modlistName) })
      .then(() => groupLayoutRetry.succeeded())
      .catch(error => groupLayoutRetry.failed(serialized, () =>
        pushUiError({ title: "Could not save groups", message: "The mod-list group layout could not be persisted.", detail: String(error), severity: "error", scope: "launch" })));
  });

  createEffect(() => {
    const modlistName = selectedModListName();
    const serialized = serializeRuleGroups(aestheticGroups());

    if (!groupLayoutReady() || !modlistName || !isTauri() || serialized === lastSavedRuleMetaGroups()) return;

    setLastSavedRuleMetaGroups(serialized);
    void invoke("save_modlist_groups_command", { input: buildGroupsPayload(modlistName) })
      .then(() => ruleMetaGroupsRetry.succeeded())
      .catch(error => ruleMetaGroupsRetry.failed(serialized, () =>
        pushUiError({ title: "Could not save groups", message: "The aesthetic group layout could not be persisted.", detail: String(error), severity: "error", scope: "launch" })));
  });

  createEffect(() => {
    const modlistName = selectedModListName();
    const serialized = serializeExpandedRows(expandedRows());

    if (!groupLayoutReady() || !modlistName || !isTauri() || serialized === lastSavedCollapsedAlts()) return;

    setLastSavedCollapsedAlts(serialized);
    void invoke("save_modlist_groups_command", { input: buildGroupsPayload(modlistName) })
      .then(() => collapsedAltsRetry.succeeded())
      .catch(error => collapsedAltsRetry.failed(serialized, () =>
        pushUiError({ title: "Could not save groups", message: "The alt panel state could not be persisted.", detail: String(error), severity: "error", scope: "launch" })));
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
    }).then(() => {
      incompatRetry.succeeded();
      void runResolution();
    }).catch(error => incompatRetry.failed(serialized, () =>
      pushUiError({ title: "Could not save incompatibilities", message: "The incompatibility rules could not be persisted.", detail: String(error), severity: "error", scope: "launch" })));
  });

  // Confrontato in termini di mod id: un riordino che rinumera solo gli id di
  // riga produce lo stesso batch e non riscrive rules.json.
  const serializeAdvanced = () => JSON.stringify(buildAdvancedPayload(savedLinks(), versionRules(), customConfigs()).payload);

  createEffect(() => {
    const modlistName = selectedModListName();
    const links = savedLinks();
    const vr = versionRules();
    const cc = customConfigs();

    if (!linksReady() || !advancedReady() || !modlistName || !isTauri()) return;

    const { payload, skipped } = buildAdvancedPayload(links, vr, cc);
    if (skipped.size > 0) {
      // Not saved, and `lastSavedAdvanced` stays: a partial batch would wipe
      // the advanced data of those rows on disk (review-frontend-stato-19).
      logger.warn("App", "advanced batch not saved: entries name rows the editor no longer has", { rowIds: [...skipped] });
      return;
    }
    const serialized = JSON.stringify(payload);
    if (serialized === lastSavedAdvanced()) return;

    setLastSavedAdvanced(serialized);
    setLastSavedLinks(serializeLinks(links));

    void invoke("save_advanced_batch_command", {
      input: { modlistName, ...payload },
    }).then(() => {
      advancedRetry.succeeded();
      // A local jar with one Modrinth alternative is a pin or not depending on
      // its "Only when" rule (`mod_version_pin::pin_kind`): the rows are read
      // again so the pin the editor shows is the backend's
      // (review-frontend-stato-20). Otherwise the resolution is enough.
      if (untrack(mayBeLegacyPin)) void loadEditorSnapshot(modlistName);
      else void runResolution();
    }).catch(error => advancedRetry.failed(serialized, () =>
      pushUiError({ title: "Could not save rule config", message: "Failed to persist advanced config.", detail: String(error), severity: "error", scope: "launch" })));
  });

  const syncPersistedState = () => {
    setLastSavedGroupLayout(serializeGroupsLayout(functionalGroups()));
    setLastSavedRuleMetaGroups(serializeRuleGroups(aestheticGroups()));
    setLastSavedCollapsedAlts(serializeExpandedRows(expandedRows()));
    setLastSavedIncompat(serializeIncompatibilities(savedIncompatibilities()));
    setLastSavedLinks(serializeLinks(savedLinks()));
    setLastSavedAdvanced(serializeAdvanced());
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
    // Un nuovo tentativo appartiene alla mod list che si sta lasciando.
    for (const retry of allRetries) retry.cancel();
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
