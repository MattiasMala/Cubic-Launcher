// ── Mod-list data types ───────────────────────────────────────────────────────

export type ModRow = {
  id: string;
  name: string;
  /** Modrinth project slug — present for Modrinth mods, absent for local mods. Used for icon fetching. */
  modrinth_id?: string;
  /** `data:image/png;base64,…` extracted from a local mod's jar, absent for Modrinth mods and for jars without an icon. */
  iconImage?: string;
  /** First mod ID in the rule (used as stable link target identifier). */
  primaryModId?: string;
  kind: "modrinth" | "local";
  /** Whether this rule is enabled (disabled mods are ignored by the resolver). */
  enabled: boolean;
  area: string;
  note: string;
  tags: string[];
  alternatives?: ModRow[];
  /** Primary mod IDs of linked rules (as stored in rules.json). */
  links?: string[];
  /** Visual groups inside this row's alternatives panel. */
  altGroups?: Array<{ id: string; name: string; collapsed: boolean; blockIds: string[] }>;
};

export type ModListCard = {
  name: string;
  displayName?: string;
  status: "Ready" | "Resolving" | "Offline";
  accent: string;
  description: string;
  iconImage?: string;
  iconLabel?: string;
  iconAccent?: string;
  mcVersion?: string;
  modLoader?: string;
};

export type ModrinthResult = {
  id: string;
  name: string;
  author: string;
  description: string;
  categories: string[];
  iconUrl?: string;
  downloads?: number;
};

export type AestheticGroup = {
  id: string;
  name: string;
  collapsed: boolean;
  blockIds: string[];
  scopeRowId?: string | null;
};

export type FunctionalGroup = {
  id: string;
  name: string;
  tone: string;
  modIds: string[];
};

export type IncompatibilityRule = {
  winnerId: string;
  loserId: string;
};

export type LinkRule = {
  /** The mod that requires the other */
  fromId: string;
  /** The mod being required */
  toId: string;
};

export type VersionRule = {
  id: string;
  modId: string;
  kind: 'exclude' | 'only';
  mcVersions: string[];
  loader: string;
};

export type CustomConfig = {
  id: string;
  modId: string;
  mcVersions: string[];
  loader: string;
  targetPath: string;
  files: string[];
};

export type DownloadProgressItem = {
  filename: string;
  progress: number;
  status: "queued" | "downloading" | "complete";
};

export type LauncherUiError = {
  id: string;
  title: string;
  message: string;
  detail: string;
  severity: "warning" | "error";
  scope: "launch" | "download" | "account";
};

/** Why an account is online or not (F1): mirrors `CredentialState` in `token_storage.rs`. */
export type AccountCredentials = "usable" | "signed_out" | "unreadable" | "keyring_unavailable";

export type AccountSummary = {
  id: string;
  gamertag: string;
  email: string;
  avatarUrl?: string;
  status: "online" | "offline";
  lastMode: "microsoft" | "offline";
  /** Absent for accounts the backend has not described (only the active one is). */
  credentials?: AccountCredentials;
  credentialsDetail?: string | null;
};

export type LaunchResolutionStage = {
  label: string;
  detail: string;
  progress: number;
};

// ── Tauri IPC payloads ────────────────────────────────────────────────────────

export type ActiveAccountSnapshot = {
  microsoft_id: string;
  xbox_gamertag?: string | null;
  avatar_url?: string | null;
  status: "online" | "offline";
  last_mode: "microsoft" | "offline";
  credentials: AccountCredentials;
  credentials_detail?: string | null;
};

export type ShellSnapshot = {
  modlists: Array<{
    name: string;
    description: string;
    author?: string | null;
    rule_count: number;
  }>;
  active_account?: ActiveAccountSnapshot | null;
  global_settings: {
    min_ram_mb: number;
    max_ram_mb: number;
    custom_jvm_args: string;
    profiler_enabled: boolean;
    update_notifications_enabled: boolean;
    update_notifications_resource_packs: boolean;
    update_notifications_data_packs: boolean;
    update_notifications_shaders: boolean;
    wrapper_command: string;
    java_path_override: string;
  };
  selected_modlist_overrides: {
    modlist_name?: string | null;
    min_ram_mb?: number | null;
    max_ram_mb?: number | null;
    custom_jvm_args?: string | null;
    profiler_enabled?: boolean | null;
    wrapper_command?: string | null;
    minecraft_version?: string | null;
    mod_loader?: string | null;
  };
};

export type EditorSnapshot = {
  modlist_name: string;
  rows: ModRow[];
  incompatibilities: IncompatibilityRule[];
  groups: Array<{ id: string; name: string; collapsed: boolean; blockIds: string[] }>;
};

export type LaunchProgressEvent = {
  state: "idle" | "resolving" | "ready" | "running";
  progress: number;
  stage: string;
  detail: string;
};

export type ProcessLogEvent = {
  stream: "stdout" | "stderr";
  line: string;
};

export type ProcessExitEvent = {
  success: boolean;
  exitCode?: number | null;
};

/** One row of `update_precheck_command`'s `updates` (`launch_preview_precheck.rs:40-58`). */
export type ModUpdateRow = {
  /** The mod actually loaded for this target — the alternative, when a group resolved through one (D9). */
  modId: string;
  /** Canonical Modrinth project id: the key for `modIcons()` and the name cache. */
  projectId: string;
  currentVersionId: string;
  /** `null` when Modrinth no longer returns the registered version, or when the number lookup failed (D24). */
  currentVersionNumber: string | null;
  candidateVersionId: string;
  candidateVersionNumber: string;
};

/**
 * One row of `contentUpdates` (`launch_preview_precheck_content.rs:43-57`).
 *
 * `currentVersionNumber` is not nullable, unlike the mod row's: a pack's
 * installed version is recognised inside the entry's own version list, so the
 * number comes from the same response as the candidate and has no separate
 * lookup to fail.
 */
export type ContentUpdateRow = {
  /** `resourcepack`, `shader` or `datapack`: the grouping, and half of the row's identity. */
  category: string;
  /** The Modrinth slug as written in the category's JSON file. */
  entryId: string;
  projectId: string;
  currentVersionId: string;
  currentVersionNumber: string;
  candidateVersionId: string;
  candidateVersionNumber: string;
};

/** An entry Modrinth has no version of for this target (D60). */
export type ContentEntryWithoutVersions = {
  category: string;
  entryId: string;
};

/** An entry whose version lookup failed — not the same thing as having none (D63). */
export type ContentLookupFailure = {
  category: string;
  entryId: string;
  error: string;
};

/** `update_precheck_command`'s payload (`launch_preview_precheck.rs:70-108`). */
export type UpdatePrecheckResult = {
  updates: ModUpdateRow[];
  /** `mod_id → version_id` for every selected Modrinth mod, updated or not (D16, D17). */
  resolved: Record<string, string>;
  /** Set when the version-number lookup failed; costs the "from" labels and nothing else. */
  versionNumberLookupError: string | null;
  contentUpdates: ContentUpdateRow[];
  /** `category → (entry id → version id)` for every selected Modrinth pack (D16, D17, D58). */
  resolvedContent: Record<string, Record<string, string>>;
  contentWithoutVersions: ContentEntryWithoutVersions[];
  contentLookupFailures: ContentLookupFailure[];
};

/** One screenshot found under an instance's `screenshots/` folder. */
export type ScreenshotEntry = {
  path: string;
  fileName: string;
  modlistName: string;
  instanceName: string;
  /** Filesystem mtime, not the date in the filename. */
  modifiedMs: number;
  sizeBytes: number;
};

export type ScreenshotListing = {
  entries: ScreenshotEntry[];
  /** Whether this platform has a system trash at all; the attempt can still fail. */
  trashSupported: boolean;
};

/** How `GameType` in `level.dat` reads; `unknown` is a mode this build does not know. */
export type WorldGameMode = "survival" | "creative" | "adventure" | "spectator" | "unknown";

/**
 * One way into a world: an instance, and the name that instance's own
 * `saves/` uses for it.
 *
 * The two names come apart as soon as a name is taken (D98): a world shared
 * into an instance that already has a `New World` is linked under
 * `New World shared`, or `New World shared (2)`. `folderName` is the one to
 * pass to Quick Play and to the unshare command.
 */
export type WorldInstanceLink = {
  instanceName: string;
  folderName: string;
};

/**
 * One singleplayer world.
 *
 * The identity is the id, never the name: two worlds can be called
 * `New World` and live in a folder called `New World` in two different
 * instances, which is exactly the case on this machine.
 */
export type WorldEntry = {
  modlistName: string;
  /**
   * The instance the world lives in, or **`null` for a shared world** (D94),
   * which lives in the mod list's own `worlds/` folder and belongs to no
   * single instance.
   */
  instanceName: string | null;
  /** The world's own folder name: in its instance, or in `worlds/`. */
  folderName: string;
  /** `LevelName`: what the player sees in game, and not the folder name. */
  levelName: string;
  gameMode: WorldGameMode;
  lastPlayedMs: number;
  /** Absolute path of `icon.png`, when the world has one. Usually it has not. */
  iconPath: string | null;
  /** Hidden from "Jump in" right now (D65); playing it again brings it back. */
  hidden: boolean;
  /**
   * Every instance that can open this world, sorted by name. One entry for an
   * ordinary world — itself — and one per link for a shared one.
   *
   * **Empty is a real state**: a shared world every instance has dropped stays
   * in the mod list with nothing able to open it, and the card has to say so
   * rather than offer a Play that cannot work.
   */
  instances: WorldInstanceLink[];
};

// ── Skins (E7) — the shapes `skins.rs` serializes ─────────────────────────────

/** `unknown` is an arm model Mojang may add tomorrow; the library only saves the two. */
export type SkinVariant = "classic" | "slim" | "unknown";

/**
 * Where a card comes from: the player's library, the eighteen vanilla
 * defaults, or the skin worn on the profile that is in neither.
 */
export type SkinSource = "saved" | "default" | "external";

export type SkinCard = {
  source: SkinSource;
  textureKey: string;
  variant: SkinVariant;
  name: string | null;
  /** Worn on the profile right now. */
  active: boolean;
  /** A data URL for saved and default cards (works offline); Mojang's https URL for an external one. */
  textureUrl: string;
};

export type CapeCard = {
  id: string;
  alias: string | null;
  /** Always Mojang's https URL: without a network a cape can't be drawn. */
  textureUrl: string;
  active: boolean;
};

export type SkinErrorKind = "notSignedIn" | "rateLimited" | "invalidSkin" | "network" | "mojang" | "library";

/** Every skin command rejects with this object, never a string: read `kind`, not `message`. */
export type SkinError = {
  kind: SkinErrorKind;
  message: string;
  status: number | null;
  retryAfterSeconds: number | null;
};

export type SkinsView = {
  playerUuid: string;
  playerName: string | null;
  /** External first, then saved (newest first), then the defaults. */
  skins: SkinCard[];
  capes: CapeCard[];
  /** Mojang did not answer: the library is shown anyway and no card is active. */
  profileError: SkinError | null;
};

// ── Static constants ──────────────────────────────────────────────────────────

export const MOD_LOADERS = ["Fabric", "NeoForge", "Forge", "Vanilla"] as const;
export const DEFAULT_MOD_LOADER = "Fabric";

export function normalizeModLoader(loader?: string | null): string {
  return MOD_LOADERS.includes(loader as typeof MOD_LOADERS[number])
    ? loader!
    : DEFAULT_MOD_LOADER;
}

/**
 * Split an instance directory name back into the pair it was built from.
 *
 * The backend names an instance `<minecraft version>-<loader>`
 * (`launch_preview_runtime.rs:build_instance_root`), and a Minecraft version
 * can itself contain a dash — `1.20.1-pre1-fabric` — so the split is on the
 * **last** one. An unknown loader suffix means this directory was not built
 * by us: the caller gets `null` and refuses rather than launching something
 * it guessed.
 */
export function parseInstanceName(
  instanceName: string,
): { minecraftVersion: string; modLoader: string } | null {
  const separator = instanceName.lastIndexOf("-");
  if (separator <= 0 || separator === instanceName.length - 1) return null;

  const suffix = instanceName.slice(separator + 1).toLowerCase();
  const modLoader = MOD_LOADERS.find(loader => loader.toLowerCase() === suffix);
  if (!modLoader) return null;

  return { minecraftVersion: instanceName.slice(0, separator), modLoader };
}

export const LAUNCH_STAGES: LaunchResolutionStage[] = [
  { label: "Resolve Rules",   detail: "Evaluating Mod-list rules, exclusions and fallback order.", progress: 18 },
  { label: "Check Cache",     detail: "Inspecting cached JARs and dependency records before download planning.", progress: 41 },
  { label: "Prepare Instance",detail: "Refreshing symlinks, configs and launch metadata for the selected target.", progress: 73 },
  { label: "Launch Ready",    detail: "Java runtime, loader profile and launch command are ready to hand off.", progress: 100 },
];
