// Reusable store mutations.
//
// Keep state writes here when they are shared across components. This keeps
// `store-state.ts` limited to signal declarations and makes optimistic UI
// updates easier to audit.

import { createSignal } from "solid-js";
import type { CustomConfig, LauncherUiError, VersionRule } from "./lib/types";
import {
  aestheticGroups,
  draftLinks,
  editingGroupId,
  functionalGroups,
  functionalGroupTone,
  groupNameDraft,
  newFunctionalGroupName,
  savedIncompatibilities,
  savedLinks,
  selectedIds,
  setAestheticGroups,
  setAlternativesPanelParentId,
  setCustomConfigs,
  setDownloadItems,
  setDraftIncompatibilities,
  setDraftLinks,
  setEditingGroupId,
  setExpandedRows,
  setFunctionalGroups,
  setFunctionalGroupModalOpen,
  setGroupNameDraft,
  setIncompatibilityFocusId,
  setIncompatibilityModalOpen,
  setLauncherErrors,
  setLaunchLogs,
  setLaunchProgress,
  setLaunchStageDetail,
  setLaunchStageLabel,
  setLaunchState,
  setLinkModalModIds,
  setLinkModalOpen,
  setNewFunctionalGroupName,
  setSavedIncompatibilities,
  setSavedLinks,
  setSelectedIds,
  setTagFilter,
  setVersionRules,
} from "./store-state";
import {
  parentIdByChildId,
} from "./store-selectors";
import { LAUNCH_STAGES } from "./lib/types";

export function upsertDownloadProgress(update: { filename: string; progress: number }) {
  setDownloadItems(current => {
    const exists = current.some(item => item.filename === update.filename);
    const next = {
      filename: update.filename,
      progress: update.progress,
      status: update.progress >= 100 ? "complete" as const : "downloading" as const,
    };
    if (exists) {
      return current.map(item => item.filename === update.filename ? next : item);
    }
    return [...current, next];
  });
}

// Ids have to be unique per notice, not per millisecond: a single launch can
// push several notices inside the same tick (one per mod left out), and two
// rows sharing an id make the banner dismiss both at once.
let uiErrorSequence = 0;

export function pushUiError(error: Omit<LauncherUiError, "id">) {
  uiErrorSequence += 1;
  setLauncherErrors(current => [{ id: `ui-error-${Date.now()}-${uiErrorSequence}`, ...error }, ...current.slice(0, 19)]);
}

export function dismissUiError(id: string) {
  setLauncherErrors(current => current.filter(error => error.id !== id));
}

export function dismissAllUiErrors() {
  setLauncherErrors([]);
}

export function toggleExpanded(id: string) {
  setExpandedRows(current => current.includes(id) ? current.filter(entry => entry !== id) : [...current, id]);
}

export function toggleSelected(id: string) {
  setSelectedIds(current => current.includes(id) ? current.filter(entry => entry !== id) : [...current, id]);
}

export function toggleGroupCollapsed(id: string) {
  setAestheticGroups(current => current.map(group => group.id === id ? { ...group, collapsed: !group.collapsed } : group));
}

function normalizedGroupName(name: string) {
  return name.trim().toLocaleLowerCase();
}

function aestheticGroupNameExists(name: string, excludeId?: string) {
  const normalized = normalizedGroupName(name);
  return aestheticGroups().some(group => group.id !== excludeId && normalizedGroupName(group.name) === normalized);
}

function functionalGroupNameExists(name: string, excludeId?: string) {
  const normalized = normalizedGroupName(name);
  return functionalGroups().some(group => group.id !== excludeId && normalizedGroupName(group.name) === normalized);
}

export function startGroupRename(id: string, name: string) {
  setEditingGroupId(id);
  setGroupNameDraft(name);
}

export function nextAestheticGroupName(scopeRowId: string | null = null) {
  const usedNumbers = new Set(
    aestheticGroups()
      .filter(group => (group.scopeRowId ?? null) === scopeRowId)
      .filter(group => group.blockIds.length > 0)
      .map(group => /^Group\s+(\d+)$/i.exec(group.name.trim()))
      .filter((match): match is RegExpExecArray => Boolean(match))
      .map(match => Number(match[1]))
      .filter(number => Number.isFinite(number) && number > 0)
  );

  let next = 1;
  while (usedNumbers.has(next)) next += 1;
  return `Group ${next}`;
}

export function selectionContext(): "top-level" | "same-parent" | "mixed" | "empty" {
  const ids = selectedIds();
  if (ids.length === 0) return "empty";
  const parents = new Set(ids.map(id => parentIdByChildId().get(id) ?? null));
  if (parents.size !== 1) return "mixed";
  const parent = [...parents][0];
  return parent === null ? "top-level" : "same-parent";
}

export function commitGroupRename(id: string) {
  const name = groupNameDraft().trim();
  if (!name) {
    setEditingGroupId(null);
    return;
  }
  if (aestheticGroupNameExists(name, id)) {
    pushUiError({
      title: "Duplicate group name",
      message: `A visual group named '${name}' already exists.`,
      detail: "Choose a unique visual group name.",
      severity: "warning",
      scope: "launch",
    });
    return;
  }
  setAestheticGroups(current => current.map(group => group.id === id ? { ...group, name } : group));
  setEditingGroupId(null);
}

export function createAestheticGroup() {
  const context = selectionContext();
  if (context === "empty") {
    pushUiError({
      title: "No mods selected",
      message: "Select one or more mods before creating a visual group.",
      detail: "Visual groups are created from the currently selected mods.",
      severity: "warning",
      scope: "launch",
    });
    return;
  }
  if (context === "mixed") {
    pushUiError({
      title: "Mixed selection",
      message: "A visual group can only contain mods from the same level.",
      detail: "Select top-level mods only, or alternatives that share the same direct parent.",
      severity: "warning",
      scope: "launch",
    });
    return;
  }

  const selected = selectedIds();
  const scopeRowId = context === "same-parent" ? (parentIdByChildId().get(selected[0]) ?? null) : null;
  const id = `ag-${Date.now()}`;
  const name = nextAestheticGroupName(scopeRowId);

  setAestheticGroups(current => {
    const withoutSelected = current.map(group => ({
      ...group,
      blockIds: group.blockIds.filter(blockId => !selected.includes(blockId)),
    }));
    return [...withoutSelected, { id, name, collapsed: false, blockIds: selected, scopeRowId }];
  });
  startGroupRename(id, name);
}

export function removeRowsFromAestheticGroups(rowIds: string[]) {
  setAestheticGroups(current => current
    .map(group => ({ ...group, blockIds: group.blockIds.filter(blockId => !rowIds.includes(blockId)) }))
    .filter(group => group.blockIds.length > 0)
  );
}

export function removeAestheticGroup(id: string) {
  setAestheticGroups(current => current.filter(group => group.id !== id));
  if (editingGroupId() === id) setEditingGroupId(null);
}

export function toggleTagFilter(groupId: string) {
  setTagFilter(current => {
    const next = new Set(current);
    if (next.has(groupId)) next.delete(groupId);
    else next.add(groupId);
    return next;
  });
}

export function removeFunctionalGroupMember(groupId: string, modId: string) {
  setFunctionalGroups(current =>
    current
      .map(group => group.id !== groupId ? group : { ...group, modIds: group.modIds.filter(id => id !== modId) })
      .filter(group => group.modIds.length > 0)
  );
  if (!functionalGroups().some(group => group.id === groupId)) {
    setTagFilter(current => {
      const next = new Set(current);
      next.delete(groupId);
      return next;
    });
  }
}

export function createFunctionalGroup() {
  const name = newFunctionalGroupName().trim();
  if (!name || selectedIds().length === 0) return;

  if (functionalGroupNameExists(name)) {
    pushUiError({
      title: "Duplicate tag name",
      message: `A functional group named '${name}' already exists.`,
      detail: "Functional group names must be unique.",
      severity: "warning",
      scope: "launch",
    });
    return;
  }

  const context = selectionContext();
  if (context === "mixed") {
    pushUiError({
      title: "Mixed selection",
      message: "A functional tag can only contain mods at the same level.",
      detail: "Select either top-level rules only, or alternatives of the same parent rule only.",
      severity: "warning",
      scope: "launch",
    });
    return;
  }

  setFunctionalGroups(current => [
    ...current,
    { id: `fg-${Date.now()}`, name, tone: functionalGroupTone(), modIds: selectedIds() },
  ]);
  setFunctionalGroupModalOpen(false);
  setNewFunctionalGroupName("");
}

export function removeIncompatibility(modAId: string, modBId: string) {
  setSavedIncompatibilities(current => current.filter(rule =>
    !((rule.winnerId === modAId && rule.loserId === modBId) ||
      (rule.winnerId === modBId && rule.loserId === modAId))
  ));
}

export function openIncompatibilityEditor() {
  if (selectedIds().length === 0) return;
  setDraftIncompatibilities(savedIncompatibilities().map(rule => ({ ...rule })));
  setIncompatibilityFocusId(selectedIds()[0]);
  setIncompatibilityModalOpen(true);
}

export function openLinkModal() {
  const ids = selectedIds();
  if (ids.length < 2) return;
  setDraftLinks(savedLinks().map(link => ({ ...link })));
  setLinkModalModIds([...ids]);
  setLinkModalOpen(true);
}

export function toggleDraftLink(fromId: string, toId: string) {
  setDraftLinks(current => {
    const hasLink = current.some(link => link.fromId === fromId && link.toId === toId);
    if (hasLink) return current.filter(link => !(link.fromId === fromId && link.toId === toId));
    return [...current, { fromId, toId }];
  });
}

export function saveDraftLinks() {
  setSavedLinks(draftLinks().map(link => ({ ...link })));
  setLinkModalOpen(false);
}

export function removeLink(fromId: string, toId: string) {
  setSavedLinks(current => current.filter(link => !(
    (link.fromId === fromId && link.toId === toId) ||
    (link.fromId === toId && link.toId === fromId)
  )));
}

export function cycleLinkDirection(fromId: string, partnerId: string) {
  setSavedLinks(current => {
    const hasAB = current.some(link => link.fromId === fromId && link.toId === partnerId);
    const hasBA = current.some(link => link.fromId === partnerId && link.toId === fromId);
    const without = current.filter(link =>
      !((link.fromId === fromId && link.toId === partnerId) || (link.fromId === partnerId && link.toId === fromId))
    );

    if (hasAB && !hasBA) {
      return [...without, { fromId, toId: partnerId }, { fromId: partnerId, toId: fromId }];
    }
    if (hasAB && hasBA) {
      return [...without, { fromId: partnerId, toId: fromId }];
    }
    return [...without, { fromId, toId: partnerId }];
  });
}

export function setPairConflictEnabled(baseId: string, otherId: string, enabled: boolean) {
  setDraftIncompatibilities(current => {
    const without = current.filter(rule =>
      !((rule.winnerId === baseId && rule.loserId === otherId) || (rule.winnerId === otherId && rule.loserId === baseId))
    );
    if (!enabled) return without;
    return [...without, { winnerId: baseId, loserId: otherId }];
  });
}

export function setPairWinner(baseId: string, otherId: string, winnerId: string) {
  setDraftIncompatibilities(current => {
    const without = current.filter(rule =>
      !((rule.winnerId === baseId && rule.loserId === otherId) || (rule.winnerId === otherId && rule.loserId === baseId))
    );
    return [...without, { winnerId, loserId: winnerId === baseId ? otherId : baseId }];
  });
}

export function dismissError(id: string) {
  setLauncherErrors(current => current.filter(error => error.id !== id));
}

export function openAlternativesPanel(rowId: string) {
  setAlternativesPanelParentId(rowId);
}

export const [onToggleEnabled, setOnToggleEnabled] = createSignal<((rowId: string | string[], enabled: boolean) => void) | null>(null);

export function resetLaunchUiState() {
  setLaunchState("idle");
  setLaunchProgress(0);
  setLaunchStageLabel(LAUNCH_STAGES[0].label);
  setLaunchStageDetail(LAUNCH_STAGES[0].detail);
  setLaunchLogs([]);
  setDownloadItems([]);
}

export function addModToFunctionalGroup(groupId: string, modId: string) {
  setFunctionalGroups(current => current.map(group =>
    group.id === groupId && !group.modIds.includes(modId)
      ? { ...group, modIds: [...group.modIds, modId] }
      : group
  ));
}

export function createFunctionalGroupForMod(name: string, modId: string) {
  const trimmed = name.trim();
  if (!trimmed) return;
  setFunctionalGroups(current => [...current, { id: `fg-${Date.now()}`, name: trimmed, tone: "violet", modIds: [modId] }]);
}

export function addVersionRule(rule: Omit<VersionRule, "id">) {
  setVersionRules(current => [...current, { ...rule, id: `vr-${Date.now()}` }]);
}

export function removeVersionRule(id: string) {
  setVersionRules(current => current.filter(rule => rule.id !== id));
}

export function updateVersionRule(id: string, patch: Partial<Omit<VersionRule, "id" | "modId">>) {
  setVersionRules(current => current.map(rule => rule.id === id ? { ...rule, ...patch } : rule));
}

export function addCustomConfig(modId: string) {
  setCustomConfigs(current => [...current, { id: `cc-${Date.now()}`, modId, mcVersions: [], loader: "any", targetPath: "", files: [] }]);
}

export function removeCustomConfig(id: string) {
  setCustomConfigs(current => current.filter(config => config.id !== id));
}

export function updateCustomConfig(id: string, patch: Partial<Omit<CustomConfig, "id" | "modId">>) {
  setCustomConfigs(current => current.map(config => config.id === id ? { ...config, ...patch } : config));
}
