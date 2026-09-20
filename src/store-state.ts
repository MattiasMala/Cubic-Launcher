// Raw SolidJS store state.
//
// Keep signal declarations here and export them through `store.ts`. Derived
// reads belong in `store-selectors.ts`; writes and reusable mutations belong in
// `store-actions.ts`.

import { createSignal } from "solid-js";
import type {
  AccountSummary,
  AestheticGroup,
  CustomConfig,
  DownloadProgressItem,
  FunctionalGroup,
  IncompatibilityRule,
  LauncherUiError,
  LinkRule,
  ModListCard,
  ModrinthResult,
  ModRow,
  UpdatePrecheckResult,
  VersionRule,
} from "./lib/types";
import { LAUNCH_STAGES } from "./lib/types";

export { LAUNCH_STAGES } from "./lib/types";
export type UpdateInfo = {
  version: string;
  notes: string | null;
  currentVersion: string;
};
export type UpdateCheckState = "idle" | "checking" | "upToDate" | "available" | "error";
export type UpdateCheckResponse = {
  status: "upToDate" | "available" | "error";
  currentVersion: string;
  version: string | null;
  notes: string | null;
};

export const DEMO_MOD_LISTS: ModListCard[] = [
  { name: "My First Pack", status: "Ready", accent: "from-primary/30 via-primary/10 to-transparent", description: "Create a mod list by clicking + above." },
];

export const DEMO_ACCOUNTS: AccountSummary[] = [
  { id: "demo", gamertag: "Not logged in", email: "", status: "offline", lastMode: "offline" },
];

export const MOCK_MODRINTH: ModrinthResult[] = [
  { id: "sodium", name: "Sodium", author: "jellysquid3", description: "Modern rendering optimization mod with strong Fabric support.", categories: ["Optimization", "Rendering"] },
  { id: "iris", name: "Iris Shaders", author: "coderbot", description: "Shader support compatible with Sodium.", categories: ["Shaders", "Rendering"] },
  { id: "xaeros-minimap", name: "Xaero's Minimap", author: "xaero96", description: "Compact minimap for utility and navigation.", categories: ["Utility", "Map"] },
  { id: "fabric-api", name: "FabricMC", author: "FabricMC", description: "Core modding API - almost always required by Fabric mods.", categories: ["Library", "Core"] },
  { id: "lithium", name: "Lithium", author: "jellysquid3", description: "General-purpose optimization mod.", categories: ["Optimization"] },
  { id: "create", name: "Create", author: "simibubi", description: "Automation and aesthetics with rotating contraptions.", categories: ["Content", "Tech"] },
  { id: "journeymap", name: "JourneyMap", author: "techbrew", description: "Real-time in-game mapping.", categories: ["Utility", "Map"] },
  { id: "modmenu", name: "Mod Menu", author: "Prospector", description: "Adds a mod menu to view installed mods.", categories: ["Utility"] },
];

export const [modListCards, setModListCards] = createSignal<ModListCard[]>([]);
export const [selectedModListName, setSelectedModListName] = createSignal<string>("");
/**
 * Which destination of the icon rail is showing. "home" is the Modrinth-style
 * landing page — the worlds to jump into, then the mod list library;
 * "modlist" is the editor plus its launch panel, which is where picking a mod
 * list lands; "screenshots" is the global gallery. The Skin icon is a
 * placeholder and has no view (E7).
 */
export type RailView = "home" | "modlist" | "screenshots";
export const [activeRailView, setActiveRailView] = createSignal<RailView>("home");
/**
 * The world folder the next launch should open directly, or `null` for the
 * ordinary Play that stops at the menu. Set by the Play button of a world
 * card and read by the launch request; it is a signal and not an argument
 * because the update popup can sit between the click and the launch.
 */
export const [quickPlayWorldFolder, setQuickPlayWorldFolder] = createSignal<string | null>(null);
/** The Library search box on the home: filters the mod list grid by name. */
export const [librarySearch, setLibrarySearch] = createSignal("");
export const [search, setSearch] = createSignal("");
export const [minecraftVersions, setMinecraftVersions] = createSignal<string[]>(["1.21.1", "1.20.6", "1.20.4", "1.19.4"]);
export const [mcWithSnapshots, setMcWithSnapshots] = createSignal<string[]>([]);
export const [showSnapshots, setShowSnapshots] = createSignal(false);
export const [selectedMcVersion, setSelectedMcVersion] = createSignal("1.21.1");
export const [selectedModLoader, setSelectedModLoader] = createSignal<string>("Fabric");
export const [launchState, setLaunchState] = createSignal<"idle" | "resolving" | "ready" | "running">("idle");
/** True while `update_precheck_command` runs, i.e. between Play and the popup. */
export const [updateCheckRunning, setUpdateCheckRunning] = createSignal(false);
/** The pre-check payload the popup is showing; `null` means no popup. */
export const [pendingUpdatePrecheck, setPendingUpdatePrecheck] = createSignal<UpdatePrecheckResult | null>(null);
export const [launchProgress, setLaunchProgress] = createSignal(0);
export const [launchStageLabel, setLaunchStageLabel] = createSignal(LAUNCH_STAGES[0].label);
export const [launchStageDetail, setLaunchStageDetail] = createSignal(LAUNCH_STAGES[0].detail);
export const [launchLogs, setLaunchLogs] = createSignal<string[]>([]);
export const [logViewerOpen, setLogViewerOpen] = createSignal(false);
export const [downloadItems, setDownloadItems] = createSignal<DownloadProgressItem[]>([]);
export const [launcherErrors, setLauncherErrors] = createSignal<LauncherUiError[]>([]);
export const [updateInfo, setUpdateInfo] = createSignal<UpdateInfo | null>(null);
export const [updateBusy, setUpdateBusy] = createSignal(false);
export const [updateCheckState, setUpdateCheckState] = createSignal<UpdateCheckState>("idle");
export const [errorCenterOpen, setErrorCenterOpen] = createSignal(false);
export const [accountsModalOpen, setAccountsModalOpen] = createSignal(false);
export const [accounts, setAccounts] = createSignal<AccountSummary[]>([]);
export const [activeAccountId, setActiveAccountId] = createSignal<string>("");
export const [instancePresentationOpen, setInstancePresentationOpen] = createSignal(false);
export const [verificationModalOpen, setVerificationModalOpen] = createSignal(false);
export const [instancePresentation, setInstancePresentation] = createSignal({ displayName: "", iconLabel: "ML", iconAccent: "", notes: "", iconImage: "" });
export const [exportModalOpen, setExportModalOpen] = createSignal(false);
export const [exportOptions, setExportOptions] = createSignal({ rulesJson: true, modJars: false, configFiles: false, resourcePacks: false, dataPacks: false, shaders: false, otherFiles: false, selectedOtherPaths: [] as string[] });
export const [settingsModalOpen, setSettingsModalOpen] = createSignal(false);
export const [settingsTab, setSettingsTab] = createSignal<"global" | "modlist">("global");
export const [globalSettings, setGlobalSettings] = createSignal({ minRamMb: 2048, maxRamMb: 4096, customJvmArgs: "-XX:+UseG1GC -XX:+ParallelRefProcEnabled", profilerEnabled: false, updateNotificationsEnabled: true, updateNotificationsResourcePacks: true, updateNotificationsDataPacks: true, updateNotificationsShaders: true, wrapperCommand: "", javaPathOverride: "" });
export const [modlistOverrides, setModlistOverrides] = createSignal({ minRamEnabled: false, minRamMb: 2048, maxRamEnabled: false, maxRamMb: 4096, customArgsEnabled: false, customJvmArgs: "", profilerEnabled: false, profilerActive: false, wrapperEnabled: false, wrapperCommand: "" });
export type GlobalSettingsState = ReturnType<typeof globalSettings>;
export type ModlistOverridesState = ReturnType<typeof modlistOverrides>;
export const [addModModalOpen, setAddModModalOpen] = createSignal(false);
export type ContentTabId = "mods" | "resourcepack" | "datapack" | "shader";
export const [activeContentTab, setActiveContentTab] = createSignal<ContentTabId>("mods");
export const [addModSearch, setAddModSearch] = createSignal("");
export const [addModMode, setAddModMode] = createSignal<"modrinth" | "local">("modrinth");
export const [selectedIds, setSelectedIds] = createSignal<string[]>([]);
export const [expandedRows, setExpandedRows] = createSignal<string[]>([]);
export const [modRowsState, setModRowsState] = createSignal<ModRow[]>([]);
export const [aestheticGroups, setAestheticGroups] = createSignal<AestheticGroup[]>([]);
export const [functionalGroups, setFunctionalGroups] = createSignal<FunctionalGroup[]>([]);
export type SortOrder = "default" | "name-az" | "name-za";
export const [tagFilter, setTagFilter] = createSignal<Set<string>>(new Set());
export const [sortOrder, setSortOrder] = createSignal<SortOrder>("default");
export const [editingGroupId, setEditingGroupId] = createSignal<string | null>(null);
export const [groupNameDraft, setGroupNameDraft] = createSignal("");
export const [functionalGroupModalOpen, setFunctionalGroupModalOpen] = createSignal(false);
export const [newFunctionalGroupName, setNewFunctionalGroupName] = createSignal("");
export const [functionalGroupTone, setFunctionalGroupTone] = createSignal<string>("violet");
export const [alternativesPanelParentId, setAlternativesPanelParentId] = createSignal<string | null>(null);
export const [savedIncompatibilities, setSavedIncompatibilities] = createSignal<IncompatibilityRule[]>([]);
export const [draftIncompatibilities, setDraftIncompatibilities] = createSignal<IncompatibilityRule[]>([]);
export const [incompatibilityModalOpen, setIncompatibilityModalOpen] = createSignal(false);
export const [incompatibilityFocusId, setIncompatibilityFocusId] = createSignal<string | null>(null);
export const [renameRuleModalOpen, setRenameRuleModalOpen] = createSignal(false);
export const [renameRuleTargetId, setRenameRuleTargetId] = createSignal<string | null>(null);
export const [renameRuleDraft, setRenameRuleDraft] = createSignal("");
export const [createModlistModalOpen, setCreateModlistModalOpen] = createSignal(false);
export const [createModlistName, setCreateModlistName] = createSignal("");
export const [createModlistDescription, setCreateModlistDescription] = createSignal("");
export const [createModlistBusy, setCreateModlistBusy] = createSignal(false);
export const [localJarRuleName, setLocalJarRuleName] = createSignal("");
export const [appLoading, setAppLoading] = createSignal(true);
export const [modIcons, setModIcons] = createSignal<Map<string, string>>(new Map());
/** Readable Modrinth names by project id/slug, filled by `fetchMetadataForIds`. */
export const [modNames, setModNames] = createSignal<Map<string, string>>(new Map());
/**
 * `category/entryId → error` for the packs the **last pre-check** could not
 * read the versions of (D63).
 *
 * It lives here and not only in the popup payload because the popup opens
 * only when there is at least one update, and this is precisely a case where
 * there may be none: without it the failure would be invisible exactly when
 * it matters. The editor row reads it; nothing fetches anything for it.
 */
export const [contentLookupFailures, setContentLookupFailures] = createSignal<Map<string, string>>(new Map());
export const [savedLinks, setSavedLinks] = createSignal<LinkRule[]>([]);
export const [draftLinks, setDraftLinks] = createSignal<LinkRule[]>([]);
export const [linkModalOpen, setLinkModalOpen] = createSignal(false);
export const [linkModalModIds, setLinkModalModIds] = createSignal<string[]>([]);
export const [linksOverviewOpen, setLinksOverviewOpen] = createSignal(false);
export type ResolutionTargetKey = { modlistName: string; mcVersion: string; modLoader: string };
export const [resolvedTarget, setResolvedTarget] = createSignal<ResolutionTargetKey | null>(null);
export const [resolvedModIds, setResolvedModIds] = createSignal<Set<string> | null>(null);
export const [versionRules, setVersionRules] = createSignal<VersionRule[]>([]);
export const [customConfigs, setCustomConfigs] = createSignal<CustomConfig[]>([]);
export const [advancedPanelModId, setAdvancedPanelModId] = createSignal<string | null>(null);
/** Row whose "pinned version" modal is open (task B / D3). */
export const [pinModalRowId, setPinModalRowId] = createSignal<string | null>(null);
