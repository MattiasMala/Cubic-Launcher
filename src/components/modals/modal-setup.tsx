import { For, Match, Show, Switch, createEffect, createSignal, on } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import type { GlobalSettingsState, ModlistOverridesState, UpdateCheckResponse } from "../../store";
import type { AccountCredentials } from "../../lib/types";
import { loadAccounts, loadShellSnapshot } from "../../app/backend-loaders";
import {
  settingsModalOpen, setSettingsModalOpen, settingsTab, setSettingsTab,
  globalSettings, modlistOverrides,
  updateCheckState, setUpdateCheckState, setUpdateInfo,
  accountsModalOpen, setAccountsModalOpen, accounts, activeAccountId, setActiveAccountId, activeAccount,
  instancePresentationOpen, setInstancePresentationOpen,
  instancePresentation, setInstancePresentation,
  createModlistModalOpen, setCreateModlistModalOpen,
  createModlistName, setCreateModlistName,
  createModlistDescription, setCreateModlistDescription,
  createModlistBusy,
  selectedModListName,
} from "../../store";
import { Modal, ModalHeader } from "./modal-base";
import { GameOptionsPanel } from "./game-options-panel";
import { SharedFilesPanel } from "./shared-files-panel";
import { WorldsPanel } from "./worlds-panel";
import { ChevronDownIcon, ChevronRightIcon } from "../icons";
import { modlistNameProblem } from "../../lib/modlist-name";
const isTauriEnv = () => "__TAURI_INTERNALS__" in window;

export function CreateModlistModal(props: { onCreate: () => Promise<void> }) {
  const nameProblem = () => modlistNameProblem(createModlistName().trim());
  return (
    <Show when={createModlistModalOpen()}>
      <Modal onClose={() => setCreateModlistModalOpen(false)}>
        <ModalHeader title="Create New Mod List" description="Create a version-agnostic mod list that works across all Minecraft versions." onClose={() => setCreateModlistModalOpen(false)} />
        <div class="space-y-4 p-6">
          <div>
            <label class="mb-1.5 block text-sm font-medium text-foreground">Name</label>
            <input
              type="text"
              value={createModlistName()}
              onInput={e => setCreateModlistName(e.currentTarget.value)}
              placeholder="My Awesome Pack"
              class="w-full rounded-md border border-input bg-input px-3 py-2 text-sm text-foreground placeholder:text-muted-foreground focus:outline-none focus:ring-1 focus:ring-ring"
              autofocus
            />
            <Show when={nameProblem()}>
              <p class="mt-1.5 text-xs text-red-400">{nameProblem()}</p>
            </Show>
          </div>
          <div>
            <label class="mb-1.5 block text-sm font-medium text-foreground">Description</label>
            <textarea
              rows={3}
              value={createModlistDescription()}
              onInput={e => setCreateModlistDescription(e.currentTarget.value)}
              placeholder="A brief description of your mod list..."
              class="w-full resize-none rounded-md border border-input bg-input px-3 py-2 text-sm text-foreground placeholder:text-muted-foreground focus:outline-none focus:ring-1 focus:ring-ring"
            />
          </div>
        </div>
        <div class="flex justify-end gap-2 border-t border-border px-6 py-4">
          <button onClick={() => setCreateModlistModalOpen(false)} class="rounded-md bg-secondary px-4 py-2 text-sm text-secondary-foreground hover:bg-secondary/80">Cancel</button>
          <button
            onClick={() => void props.onCreate()}
            disabled={!createModlistName().trim() || nameProblem() !== null || createModlistBusy()}
            class="rounded-md bg-primary px-4 py-2 text-sm font-medium text-primary-foreground hover:bg-primary/90 disabled:opacity-50"
          >
            Create
          </button>
        </div>
      </Modal>
    </Show>
  );
}

export function SettingsModal(props: { onSave: (globalDraft: GlobalSettingsState, modlistDraft: ModlistOverridesState) => Promise<void> }) {
  const [globalDraft, setGlobalDraft] = createSignal<GlobalSettingsState>({ ...globalSettings() });
  const [modlistDraft, setModlistDraft] = createSignal<ModlistOverridesState>({ ...modlistOverrides() });
  const [lastUpdateCheck, setLastUpdateCheck] = createSignal<UpdateCheckResponse | null>(null);
  const [notificationsAdvancedOpen, setNotificationsAdvancedOpen] = createSignal(false);

  createEffect(on(settingsModalOpen, open => {
    if (!open) return;
    setGlobalDraft({ ...globalSettings() });
    setModlistDraft({ ...modlistOverrides() });
    setSettingsTab("global");
    setLastUpdateCheck(null);
    setUpdateCheckState("idle");
  }));

  const handleCancel = () => setSettingsModalOpen(false);
  const handleCheckForUpdates = async () => {
    if (!isTauriEnv() || updateCheckState() === "checking") return;

    setUpdateCheckState("checking");
    setLastUpdateCheck(null);
    try {
      const result = await invoke<UpdateCheckResponse>("check_for_updates");
      setLastUpdateCheck(result);

      if (result.status === "available" && result.version) {
        setUpdateInfo({
          version: result.version,
          notes: result.notes,
          currentVersion: result.currentVersion,
        });
        setUpdateCheckState("available");
        return;
      }

      if (result.status === "upToDate") {
        setUpdateCheckState("upToDate");
        return;
      }

      setUpdateCheckState("error");
    } catch {
      setLastUpdateCheck(null);
      setUpdateCheckState("error");
    }
  };

  const updateCheckMessage = () => {
    const state = updateCheckState();
    const result = lastUpdateCheck();
    if (state === "checking") return "Checking for updates...";
    if (state === "available" && result?.version) return `Version ${result.version} available`;
    if (state === "upToDate" && result) return `You're up to date (v${result.currentVersion})`;
    if (state === "error") return "Could not check for updates";
    return null;
  };

  const field = (label: string, content: any) => (
    <div class="rounded-md border border-border bg-background p-4">
      <p class="mb-2 text-sm font-medium text-foreground">{label}</p>
      {content}
    </div>
  );

  return (
    <Show when={settingsModalOpen()}>
      <Modal onClose={handleCancel} maxWidth="max-w-3xl">
        <ModalHeader title="Settings" description="Global defaults and Mod-list overrides" onClose={handleCancel} />
        <div class="flex flex-1 gap-0 overflow-hidden">
          <div class="w-44 shrink-0 border-r border-border p-3 space-y-1">
            {(["global", "modlist"] as const).map(tab => (
              <button
                onClick={() => setSettingsTab(tab)}
                class={`block w-full rounded-md px-3 py-2 text-left text-sm font-medium transition-colors ${settingsTab() === tab ? "bg-primary/10 text-primary" : "text-muted-foreground hover:bg-muted hover:text-foreground"}`}
              >
                {tab === "global" ? "Global Settings" : "Mod-list Overrides"}
              </button>
            ))}
          </div>

          <div class="flex-1 overflow-y-auto p-6 space-y-4">
            <Show when={settingsTab() === "global"} fallback={
              <div class="space-y-4">
                <p class="text-sm text-muted-foreground">Checked values override the global defaults for <strong class="text-foreground">{selectedModListName()}</strong>.</p>
                {[
                  { label: "Min RAM (MB)", enabled: "minRamEnabled", value: "minRamMb" },
                  { label: "Max RAM (MB)", enabled: "maxRamEnabled", value: "maxRamMb" },
                ].map(({ label, enabled, value }) => field(label,
                  <div class="flex gap-3">
                    <input type="checkbox" checked={(modlistDraft() as any)[enabled]} onChange={e => setModlistDraft(c => ({ ...c, [enabled]: e.currentTarget.checked }))} class="mt-1 h-4 w-4 rounded text-primary" />
                    <input type="number" value={(modlistDraft() as any)[value]} disabled={!(modlistDraft() as any)[enabled]} onInput={e => setModlistDraft(c => ({ ...c, [value]: Number(e.currentTarget.value) }))} class="flex-1 rounded-md border border-input bg-input px-3 py-1.5 text-sm text-foreground disabled:opacity-40 focus:outline-none focus:ring-1 focus:ring-ring" />
                  </div>
                ))}
                {field("Custom JVM Args",
                  <div class="flex gap-3">
                    <input type="checkbox" checked={modlistDraft().customArgsEnabled} onChange={e => setModlistDraft(c => ({ ...c, customArgsEnabled: e.currentTarget.checked }))} class="mt-1 h-4 w-4 rounded text-primary" />
                    <textarea rows={3} value={modlistDraft().customJvmArgs} disabled={!modlistDraft().customArgsEnabled} onInput={e => setModlistDraft(c => ({ ...c, customJvmArgs: e.currentTarget.value }))} class="flex-1 resize-none rounded-md border border-input bg-input px-3 py-1.5 text-sm text-foreground disabled:opacity-40 focus:outline-none" />
                  </div>
                )}
                {field("Profiler",
                  <div class="flex gap-3">
                    <input type="checkbox" checked={modlistDraft().profilerEnabled} onChange={e => setModlistDraft(c => ({ ...c, profilerEnabled: e.currentTarget.checked }))} class="mt-1 h-4 w-4 rounded text-primary" />
                    <label class={`flex flex-1 items-center gap-2 text-sm text-foreground ${modlistDraft().profilerEnabled ? "" : "opacity-40"}`}>
                      <button
                        type="button"
                        role="switch"
                        aria-checked={modlistDraft().profilerActive}
                        disabled={!modlistDraft().profilerEnabled}
                        data-testid="modlist-profiler-switch"
                        onClick={() => setModlistDraft(c => ({ ...c, profilerActive: !c.profilerActive }))}
                        class={`h-6 w-11 shrink-0 rounded-full border transition-colors ${
                          modlistDraft().profilerActive ? "border-primary bg-primary" : "border-border bg-muted"
                        }`}
                      >
                        <span
                          class={`block h-4 w-4 rounded-full bg-background transition-transform ${
                            modlistDraft().profilerActive ? "translate-x-6" : "translate-x-1"
                          }`}
                        />
                      </button>
                      Record a JFR profile for this mod list
                    </label>
                  </div>
                )}
                {field("Wrapper Command (Linux)",
                  <div class="flex gap-3">
                    <input type="checkbox" checked={modlistDraft().wrapperEnabled} onChange={e => setModlistDraft(c => ({ ...c, wrapperEnabled: e.currentTarget.checked }))} class="mt-1 h-4 w-4 rounded text-primary" />
                    <input type="text" value={modlistDraft().wrapperCommand} disabled={!modlistDraft().wrapperEnabled} onInput={e => setModlistDraft(c => ({ ...c, wrapperCommand: e.currentTarget.value }))} placeholder="gamemoderun mangohud" class="flex-1 rounded-md border border-input bg-input px-3 py-1.5 text-sm text-foreground placeholder:text-muted-foreground disabled:opacity-40 focus:outline-none focus:ring-1 focus:ring-ring" />
                  </div>
                )}
              </div>
            }>
              <div class="space-y-4">
                <div class="grid grid-cols-2 gap-4">
                  {field("Min RAM (MB)", <input type="number" value={globalDraft().minRamMb} onInput={e => setGlobalDraft(c => ({ ...c, minRamMb: Number(e.currentTarget.value) }))} class="w-full rounded-md border border-input bg-input px-3 py-1.5 text-sm text-foreground focus:outline-none focus:ring-1 focus:ring-ring" />)}
                  {field("Max RAM (MB)", <input type="number" value={globalDraft().maxRamMb} onInput={e => setGlobalDraft(c => ({ ...c, maxRamMb: Number(e.currentTarget.value) }))} class="w-full rounded-md border border-input bg-input px-3 py-1.5 text-sm text-foreground focus:outline-none focus:ring-1 focus:ring-ring" />)}
                </div>
                {field("Custom JVM Args", <textarea rows={3} value={globalDraft().customJvmArgs} onInput={e => setGlobalDraft(c => ({ ...c, customJvmArgs: e.currentTarget.value }))} class="w-full resize-none rounded-md border border-input bg-input px-3 py-1.5 text-sm text-foreground focus:outline-none focus:ring-1 focus:ring-ring" />)}
                {field("Java Path Override", <input type="text" value={globalDraft().javaPathOverride} onInput={e => setGlobalDraft(c => ({ ...c, javaPathOverride: e.currentTarget.value }))} placeholder="Optional explicit Java binary path" class="w-full rounded-md border border-input bg-input px-3 py-1.5 text-sm text-foreground placeholder:text-muted-foreground focus:outline-none focus:ring-1 focus:ring-ring" />)}
                {field("Wrapper Command (Linux)", <input type="text" value={globalDraft().wrapperCommand} onInput={e => setGlobalDraft(c => ({ ...c, wrapperCommand: e.currentTarget.value }))} placeholder="gamemoderun mangohud" class="w-full rounded-md border border-input bg-input px-3 py-1.5 text-sm text-foreground placeholder:text-muted-foreground focus:outline-none focus:ring-1 focus:ring-ring" />)}
                {field("Updates",
                  <div class="space-y-2">
                    <button
                      type="button"
                      onClick={() => void handleCheckForUpdates()}
                      disabled={!isTauriEnv() || updateCheckState() === "checking"}
                      class="rounded-md bg-secondary px-3 py-1.5 text-sm font-medium text-secondary-foreground transition-colors hover:bg-secondary/80 disabled:cursor-not-allowed disabled:opacity-50"
                    >
                      {updateCheckState() === "checking" ? "Checking..." : "Check for updates"}
                    </button>
                    <Show when={updateCheckMessage()}>
                      {message => (
                        <p class={`text-xs ${updateCheckState() === "error" ? "text-destructive" : updateCheckState() === "available" ? "text-primary" : "text-muted-foreground"}`}>
                          {message()}
                        </p>
                      )}
                    </Show>
                  </div>
                )}
                <div>
                  <label class="flex items-center gap-3 text-sm">
                    <input type="checkbox" checked={globalDraft().profilerEnabled} onChange={e => setGlobalDraft(c => ({ ...c, profilerEnabled: e.currentTarget.checked }))} class="h-4 w-4 rounded text-primary" />
                    <span class="text-foreground">Enable profiler globally</span>
                  </label>
                  <p class="mt-1 ml-7 text-xs text-muted-foreground">Records a Java Flight Recorder profile (profile.jfr) into the launch's log folder, and a Profile button shows it there once the game closes. Needs Java 11 or newer. If your Custom JVM Args already contain a profiling flag, yours win and nothing is added.</p>
                </div>
                <div>
                  <label class="flex items-center gap-3 text-sm">
                    <input type="checkbox" checked={globalDraft().updateNotificationsEnabled} onChange={e => setGlobalDraft(c => ({ ...c, updateNotificationsEnabled: e.currentTarget.checked }))} class="h-4 w-4 rounded text-primary" />
                    <span class="text-foreground">Notify me when updates are available</span>
                  </label>
                  <p class="mt-1 ml-7 text-xs text-muted-foreground">Cubic Launcher always launches from the versions you have. It checks Modrinth before every launch either way; when this is on and something can be updated, it stops and lets you choose what to update.</p>
                  <div class="mt-2">
                    <div class="flex items-center gap-2">
                      <span class="flex-1 text-sm text-muted-foreground ml-7">Advanced</span>
                      <button onClick={() => setNotificationsAdvancedOpen(value => !value)} class="text-muted-foreground hover:text-foreground transition-colors p-0.5" aria-label="Toggle advanced notification settings">
                        <Show when={notificationsAdvancedOpen()} fallback={<ChevronRightIcon class="h-3.5 w-3.5" />}><ChevronDownIcon class="h-3.5 w-3.5" /></Show>
                      </button>
                    </div>
                    <Show when={notificationsAdvancedOpen()}>
                      <div class="ml-7 mt-1 space-y-1.5">
                        <p class="text-xs text-muted-foreground">Mods are always included. These are the other categories to report on. A category that is off keeps the version you already have; a pack you have never installed is still installed.</p>
                        <label class="flex items-center gap-3 text-sm">
                          <input type="checkbox" checked={globalDraft().updateNotificationsResourcePacks} onChange={e => setGlobalDraft(c => ({ ...c, updateNotificationsResourcePacks: e.currentTarget.checked }))} class="h-4 w-4 rounded text-primary" />
                          <span class="text-foreground">Resource packs</span>
                        </label>
                        <label class="flex items-center gap-3 text-sm">
                          <input type="checkbox" checked={globalDraft().updateNotificationsDataPacks} onChange={e => setGlobalDraft(c => ({ ...c, updateNotificationsDataPacks: e.currentTarget.checked }))} class="h-4 w-4 rounded text-primary" />
                          <span class="text-foreground">Data packs</span>
                        </label>
                        <label class="flex items-center gap-3 text-sm">
                          <input type="checkbox" checked={globalDraft().updateNotificationsShaders} onChange={e => setGlobalDraft(c => ({ ...c, updateNotificationsShaders: e.currentTarget.checked }))} class="h-4 w-4 rounded text-primary" />
                          <span class="text-foreground">Shaders</span>
                        </label>
                      </div>
                    </Show>
                  </div>
                </div>
                {/*
                  Il terzo livello di D66. Stesso componente della scheda
                  «Game settings» di una modlist, su uno scope diverso: senza
                  di questo, «Promote to global default» scriveva un file che
                  poi non si poteva né vedere né modificare da nessuna parte.
                */}
                <div class="rounded-md border border-border bg-background p-4">
                  <p class="mb-1 text-sm font-medium text-foreground">Game settings</p>
                  <p class="mb-3 text-xs text-muted-foreground">
                    The global <code>options.txt</code>: the template new mod-lists are born
                    from. Changing it here does not touch mod-lists or instances that already
                    exist.
                  </p>
                  <GameOptionsPanel scope={{ level: "global" }} />
                </div>
              </div>
            </Show>
          </div>
        </div>
        <div class="flex justify-end gap-2 border-t border-border px-6 py-4">
          <button onClick={handleCancel} class="rounded-md bg-secondary px-4 py-2 text-sm text-secondary-foreground hover:bg-secondary/80">Cancel</button>
          <button onClick={() => void props.onSave(globalDraft(), modlistDraft())} class="rounded-md bg-primary px-4 py-2 text-sm font-medium text-primary-foreground hover:bg-primary/90">Save Settings</button>
        </div>
      </Modal>
    </Show>
  );
}

// D131: the pill names the credential state with the short form of the yellow
// box's title below the row. "offline" stays only where it means the play mode
// (the "Play offline" switch and the box texts).
const CREDENTIALS_PILL: Record<AccountCredentials, string> = {
  usable: "online",
  signed_out: "signed out",
  unreadable: "sign-in unreadable",
  keyring_unavailable: "keyring unavailable",
};

export function AccountsModal(props: { onSwitchAccount: (id: string) => Promise<void> }) {
  const [loggingIn, setLoggingIn] = createSignal(false);
  const [loginError, setLoginError] = createSignal<string | null>(null);
  const [savingOffline, setSavingOffline] = createSignal(false);
  // D134: "Remove" asks before deleting, one row at a time: the id of the row
  // waiting for confirmation, or null. Opening or closing the modal drops it.
  const [confirmRemoveId, setConfirmRemoveId] = createSignal<string | null>(null);
  const [removing, setRemoving] = createSignal(false);

  createEffect(on(accountsModalOpen, () => setConfirmRemoveId(null)));

  const removeAccount = async (microsoftId: string) => {
    if (removing()) return;
    setRemoving(true);
    try {
      await invoke("delete_account_command", { microsoftId });
      if (activeAccountId() === microsoftId) setActiveAccountId("");
      await loadAccounts();
    } catch (err) {
      setLoginError(String(err));
    } finally {
      setRemoving(false);
      setConfirmRemoveId(null);
    }
  };

  // D115: the choice is saved on the account's row and the list is reloaded
  // from the backend, so what the switch shows is what the next launch reads.
  const setPlaysOffline = async (microsoftId: string, playsOffline: boolean) => {
    setSavingOffline(true);
    try {
      await invoke("set_account_plays_offline_command", { microsoftId, playsOffline });
      await loadAccounts();
    } catch (err) {
      setLoginError(String(err));
    } finally {
      setSavingOffline(false);
    }
  };

  // `replaceUnreadable` is the explicit "Sign in again" (F1): only that button
  // lets the backend discard saved sign-ins the keyring says nobody can read.
  const signIn = async (replaceUnreadable: boolean) => {
    setLoggingIn(true);
    setLoginError(null);
    try {
      await invoke("microsoft_login_command", { replaceUnreadable });
      setAccountsModalOpen(false);
      await loadShellSnapshot(selectedModListName() || null);
    } catch (err) {
      setLoginError(String(err));
    } finally {
      setLoggingIn(false);
    }
  };

  return (
    <Show when={accountsModalOpen()}>
      <Modal onClose={() => setAccountsModalOpen(false)}>
        <ModalHeader title="Accounts" description="Microsoft login, account switching and offline mode" onClose={() => setAccountsModalOpen(false)} />
        <div class="flex-1 overflow-y-auto p-6 space-y-3">
          <div class="mb-4 rounded-md border border-border bg-muted/30 p-4">
            <p class="mb-2 text-sm font-medium text-foreground">Add Microsoft Account</p>
            <p class="mb-3 text-xs text-muted-foreground">
              The launcher opens your system browser for secure Microsoft login (OAuth 2.0 + PKCE).
              Your credentials are never seen by Cubic Launcher.
            </p>
            <button
              class="rounded-md bg-primary px-4 py-2 text-sm font-medium text-primary-foreground transition-colors hover:bg-primary/90 disabled:opacity-50"
              disabled={loggingIn()}
              onClick={() => void signIn(false)}
            >
              {loggingIn() ? "Logging in..." : "Login with Microsoft"}
            </button>
            <Show when={loginError()}>
              <p class="mt-2 text-xs text-destructive break-all">{loginError()}</p>
            </Show>
          </div>
          <p class="text-xs text-muted-foreground">Saved accounts - click to switch</p>
          <For each={accounts()}>
            {acc => (
              <div class={`rounded-md border p-4 transition-colors ${acc.id === activeAccountId() ? "border-primary/40 bg-primary/10" : "border-border hover:bg-muted/30"}`}>
                <div class="flex items-start justify-between">
                  <button
                    onClick={() => void props.onSwitchAccount(acc.id)}
                    class="flex items-start gap-3 text-left flex-1 min-w-0"
                  >
                    <Show when={acc.avatarUrl} fallback={
                      <div class="flex h-10 w-10 shrink-0 items-center justify-center rounded-full bg-primary/20 text-sm font-semibold text-primary">
                        {acc.gamertag.slice(0, 2).toUpperCase()}
                      </div>
                    }>
                      <img src={acc.avatarUrl} alt="" class="h-10 w-10 shrink-0 rounded-md" loading="lazy" />
                    </Show>
                    <div class="min-w-0">
                      <p class="font-medium text-foreground truncate">{acc.gamertag}</p>
                      <p class="text-xs text-muted-foreground truncate">{acc.email}</p>
                    </div>
                  </button>
                  <div class="flex flex-col items-end gap-1 shrink-0 ml-2">
                    <span class={`rounded-full px-2 py-0.5 text-[10px] ${acc.status === "online" ? "bg-success/15 text-success" : "bg-warning/15 text-warning"}`}>
                      {acc.credentials ? CREDENTIALS_PILL[acc.credentials] : acc.status}
                    </span>
                    <Show when={acc.id === activeAccountId()}>
                      <span class="rounded-full bg-primary/15 px-2 py-0.5 text-[10px] text-primary">Active</span>
                    </Show>
                    <div class="flex items-center gap-1.5">
                      <span class="text-[10px] text-muted-foreground">Play offline</span>
                      <button
                        role="switch"
                        aria-checked={acc.playsOffline}
                        aria-label={`Play ${acc.gamertag} offline`}
                        disabled={savingOffline()}
                        data-testid={`account-plays-offline-switch-${acc.id}`}
                        onClick={e => {
                          e.stopPropagation();
                          void setPlaysOffline(acc.id, !acc.playsOffline);
                        }}
                        class={`h-6 w-11 shrink-0 rounded-full border transition-colors ${
                          acc.playsOffline ? "border-primary bg-primary" : "border-border bg-muted"
                        }`}
                      >
                        <span
                          class={`block h-4 w-4 rounded-full bg-background transition-transform ${
                            acc.playsOffline ? "translate-x-6" : "translate-x-1"
                          }`}
                        />
                      </button>
                    </div>
                    <Show when={confirmRemoveId() === acc.id} fallback={
                      <button
                        onClick={e => {
                          e.stopPropagation();
                          setConfirmRemoveId(acc.id);
                        }}
                        class="rounded-full px-2 py-0.5 text-[10px] text-destructive hover:bg-destructive/15 transition-colors"
                      >
                        Remove
                      </button>
                    }>
                      <div class="flex items-center gap-1.5">
                        <span class="text-[10px] text-red-400">Are you sure?</span>
                        <button
                          onClick={e => {
                            e.stopPropagation();
                            void removeAccount(acc.id);
                          }}
                          disabled={removing()}
                          class="rounded-full bg-red-600 px-2 py-0.5 text-[10px] font-medium text-white hover:bg-red-700 disabled:opacity-50"
                        >
                          {removing() ? "Removing..." : "Yes, remove"}
                        </button>
                        <button
                          onClick={e => {
                            e.stopPropagation();
                            setConfirmRemoveId(null);
                          }}
                          class="rounded-full bg-secondary px-2 py-0.5 text-[10px] text-secondary-foreground hover:bg-secondary/80"
                        >
                          Cancel
                        </button>
                      </div>
                    </Show>
                  </div>
                </div>
                <Show when={acc.credentials && acc.credentials !== "usable" ? acc.credentials : undefined}>
                  {credentials => (
                    <div class="mt-3 rounded-md border border-warning/40 bg-warning/10 p-3 text-xs">
                      <Switch>
                        <Match when={credentials() === "unreadable"}>
                          <p class="font-medium text-warning">Saved sign-in can't be read</p>
                          <p class="mt-1 text-muted-foreground">
                            Games with this account start in offline mode, so servers that check
                            accounts turn you away. Sign in again to fix it: the saved sign-ins that
                            can no longer be read are discarded and replaced. The account stays in
                            this list.
                          </p>
                          <button
                            class="mt-2 rounded-md bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground transition-colors hover:bg-primary/90 disabled:opacity-50"
                            disabled={loggingIn()}
                            onClick={() => void signIn(true)}
                          >
                            {loggingIn() ? "Signing in..." : "Sign in again"}
                          </button>
                        </Match>
                        <Match when={credentials() === "signed_out"}>
                          <p class="font-medium text-warning">Not signed in</p>
                          <p class="mt-1 text-muted-foreground">
                            Games with this account start in offline mode until you sign in again.
                            Saved sign-ins of other accounts that can no longer be read, if any, are
                            discarded and replaced.
                          </p>
                          <button
                            class="mt-2 rounded-md bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground transition-colors hover:bg-primary/90 disabled:opacity-50"
                            disabled={loggingIn()}
                            onClick={() => void signIn(true)}
                          >
                            {loggingIn() ? "Signing in..." : "Sign in again"}
                          </button>
                        </Match>
                        <Match when={credentials() === "keyring_unavailable"}>
                          <p class="font-medium text-warning">System keyring unavailable</p>
                          <p class="mt-1 text-muted-foreground">
                            The launcher keeps your sign-in in the system keyring and can't reach it,
                            so games start in offline mode. Start or unlock your keyring, then reopen
                            the launcher.
                          </p>
                          <Show when={acc.credentialsDetail}>
                            <p class="mt-1 break-all font-mono text-[10px] text-muted-foreground">{acc.credentialsDetail}</p>
                          </Show>
                        </Match>
                      </Switch>
                    </div>
                  )}
                </Show>
              </div>
            )}
          </For>
        </div>
        <div class="flex justify-end border-t border-border px-6 py-4">
          <button onClick={() => setAccountsModalOpen(false)} class="rounded-md bg-secondary px-4 py-2 text-sm text-secondary-foreground hover:bg-secondary/80">Close</button>
        </div>
      </Modal>
    </Show>
  );
}


type ModlistDeletePreview = { worlds: number; screenshots: number; running: boolean; trashSupported: boolean };

const plural = (count: number, one: string, many: string) => `${count} ${count === 1 ? one : many}`;

/**
 * `onDelete(allowPermanent)` resolves to the reason the trash refused, when it
 * did (nothing was deleted then), or null. `allowPermanent` is exactly what the
 * confirmation promised, as for a screenshot (D153).
 */
export function InstancePresentationModal(props: { onSave: () => Promise<void>; onDelete: (allowPermanent: boolean) => Promise<string | null> }) {
  const [draft, setDraft] = createSignal({ ...instancePresentation() });
  const [saving, setSaving] = createSignal(false);
  const [confirmDelete, setConfirmDelete] = createSignal(false);
  const [deleting, setDeleting] = createSignal(false);
  const [deletePreview, setDeletePreview] = createSignal<ModlistDeletePreview | null>(null);
  const [deletePreviewError, setDeletePreviewError] = createSignal<string | null>(null);
  const [trashRefusal, setTrashRefusal] = createSignal<string | null>(null);
  const [tab, setTab] = createSignal<"identity" | "game" | "shared" | "worlds">("identity");

  createEffect(() => {
    if (instancePresentationOpen()) {
      setDraft({ ...instancePresentation() });
      setConfirmDelete(false);
      setTrashRefusal(null);
      setTab("identity");
    }
  });

  const close = () => {
    setInstancePresentationOpen(false);
    setConfirmDelete(false);
    setTrashRefusal(null);
  };

  const handleSave = async () => {
    if (saving()) return;
    setSaving(true);
    try {
      setInstancePresentation(draft());
      await props.onSave();
    } finally {
      setSaving(false);
    }
  };

  // D153: before asking, count what goes with the mod list and whether its
  // game is running (then the backend refuses, and the button is off).
  const askDelete = async () => {
    setDeletePreview(null);
    setDeletePreviewError(null);
    setTrashRefusal(null);
    setConfirmDelete(true);
    if (!isTauriEnv()) return;
    try {
      const { invoke: invokeCore } = await import("@tauri-apps/api/core");
      setDeletePreview(await invokeCore<ModlistDeletePreview>("modlist_delete_preview_command", { modlistName: selectedModListName() }));
    } catch (err) {
      setDeletePreviewError(String(err));
    }
  };

  const cancelDelete = () => {
    setConfirmDelete(false);
    setTrashRefusal(null);
  };

  const permanentPromised = () => trashRefusal() !== null || deletePreview()?.trashSupported === false;

  const deleteQuestion = () => {
    const preview = deletePreview();
    if (deletePreviewError()) return "Delete this mod list? Its worlds and screenshots could not be counted.";
    if (!preview) return "Counting worlds and screenshots...";
    if (preview.running) return "This mod list is being launched or played. Close the game first, then delete it.";
    const contents = `${plural(preview.worlds, "world", "worlds")} and ${plural(preview.screenshots, "screenshot", "screenshots")}`;
    const refusal = trashRefusal();
    if (refusal !== null) return `The trash could not be used (${refusal}). Delete permanently, with ${contents}? This cannot be undone.`;
    if (!preview.trashSupported) return `Delete permanently, with ${contents}? This cannot be undone.`;
    return `Move this mod list to the trash, with ${contents}? You can restore it from the trash.`;
  };

  const handleDelete = async () => {
    if (deleting()) return;
    setDeleting(true);
    let refusal: string | null = null;
    try {
      refusal = await props.onDelete(permanentPromised());
    } finally {
      setDeleting(false);
      setTrashRefusal(refusal);
      if (refusal === null) setConfirmDelete(false);
    }
  };

  const pickIcon = async () => {
    if (!isTauriEnv()) return;
    try {
      const { open } = await import("@tauri-apps/plugin-dialog");
      const { invoke: invokeCore } = await import("@tauri-apps/api/core");
      const selected = await open({
        multiple: false,
        filters: [{ name: "Images", extensions: ["png", "jpg", "jpeg", "webp", "gif"] }],
      });
      if (typeof selected === "string") {
        const dataUrl: string = await invokeCore("read_image_as_data_url_command", { path: selected });
        setDraft(cur => ({ ...cur, iconImage: dataUrl }));
      }
    } catch (_) {
      // user cancelled or no dialog support
    }
  };

  return (
    <Show when={instancePresentationOpen()}>
      <Modal onClose={close} maxWidth="max-w-4xl">
        <ModalHeader title="Settings" description="Customize the mod-list card identity and save private notes for this pack." onClose={close} />
        <div class="flex gap-1 border-b border-border px-6">
          <For each={[
            { id: "identity" as const, label: "Identity" },
            { id: "game" as const, label: "Game settings" },
            { id: "shared" as const, label: "Shared files" },
            { id: "worlds" as const, label: "Worlds" },
          ]}>
            {entry => (
              <button
                onClick={() => setTab(entry.id)}
                class={`-mb-px border-b-2 px-3 py-2 text-sm font-medium transition-colors ${
                  tab() === entry.id
                    ? "border-primary text-primary"
                    : "border-transparent text-muted-foreground hover:text-foreground"
                }`}
              >
                {entry.label}
              </button>
            )}
          </For>
        </div>
        <Show when={tab() === "game"}>
          <div class="overflow-y-auto p-6">
            <GameOptionsPanel scope={{ level: "modlist", modlist: selectedModListName() ?? "" }} />
          </div>
        </Show>
        <Show when={tab() === "shared"}>
          <div class="overflow-y-auto p-6">
            <SharedFilesPanel modlist={selectedModListName() ?? ""} />
          </div>
        </Show>
        <Show when={tab() === "worlds"}>
          <div class="overflow-y-auto p-6">
            <WorldsPanel modlist={selectedModListName() ?? ""} />
          </div>
        </Show>
        <Show when={tab() === "identity"}>
        <div class="grid gap-6 p-6 md:grid-cols-[220px,1fr]">
          <div class="rounded-lg border border-border bg-background p-4">
            <p class="mb-3 text-xs font-medium uppercase tracking-wider text-muted-foreground">Preview</p>
            <div class="flex flex-col items-center rounded-lg border border-border bg-card px-4 py-6 text-center">
              <button
                onClick={() => void pickIcon()}
                class="relative flex h-20 w-20 items-center justify-center rounded-2xl bg-primary/15 text-2xl font-semibold text-primary shadow-inner overflow-hidden hover:ring-2 hover:ring-primary transition-all"
                title="Click to set a custom icon image"
              >
                <Show when={draft().iconImage} fallback={
                  <span>{(draft().displayName || selectedModListName() || "ML").slice(0, 3).toUpperCase()}</span>
                }>
                  <img src={draft().iconImage} class="absolute inset-0 h-full w-full object-cover" alt="icon" />
                </Show>
                <span class="absolute inset-0 flex items-center justify-center bg-black/40 opacity-0 hover:opacity-100 transition-opacity text-xs text-white font-medium">Edit</span>
              </button>
              <Show when={draft().iconAccent.trim()}>
                <p class="mt-3 text-xs uppercase tracking-[0.18em] text-primary/80">
                  {draft().iconAccent}
                </p>
              </Show>
              <Show when={draft().notes.trim()}>
                <p class="mt-3 text-xs text-muted-foreground whitespace-pre-wrap break-words text-left w-full line-clamp-4">
                  {draft().notes}
                </p>
              </Show>
            </div>
          </div>

          <div class="space-y-4">
            <div>
              <label class="mb-1.5 block text-sm font-medium text-foreground">Name</label>
              <input
                type="text"
                value={draft().displayName}
                onInput={e => setDraft(cur => ({ ...cur, displayName: e.currentTarget.value }))}
                placeholder={selectedModListName()}
                class="w-full rounded-md border border-input bg-input px-3 py-2 text-sm text-foreground focus:outline-none focus:ring-1 focus:ring-ring"
                autofocus
              />
            </div>
            <div>
              <label class="mb-1.5 block text-sm font-medium text-foreground">Created by</label>
              <input
                type="text"
                value={draft().iconAccent}
                onInput={e => setDraft(cur => ({ ...cur, iconAccent: e.currentTarget.value }))}
                placeholder={activeAccount()?.gamertag || "Author name"}
                class="w-full rounded-md border border-input bg-input px-3 py-2 text-sm text-foreground focus:outline-none focus:ring-1 focus:ring-ring"
              />
            </div>
            <div>
              <label class="mb-1.5 block text-sm font-medium text-foreground">Notes</label>
              <textarea
                rows={8}
                value={draft().notes}
                onInput={e => setDraft(cur => ({ ...cur, notes: e.currentTarget.value }))}
                placeholder="Remember shader requirements, server notes, or version-specific caveats..."
                class="w-full resize-none rounded-md border border-input bg-input px-3 py-2 text-sm text-foreground focus:outline-none focus:ring-1 focus:ring-ring whitespace-pre-wrap break-words"
              />
            </div>
          </div>
        </div>
        </Show>
        <Show when={tab() === "game"}>
          <div class="flex justify-end border-t border-border px-6 py-4">
            <button onClick={close} class="rounded-md bg-secondary px-4 py-2 text-sm text-secondary-foreground hover:bg-secondary/80">Close</button>
          </div>
        </Show>
        <Show when={tab() === "identity"}>
        <div class="flex items-center justify-between gap-2 border-t border-border px-6 py-4">
          <Show when={confirmDelete()} fallback={
            <button
              onClick={() => void askDelete()}
              class="rounded-md bg-red-900/40 px-4 py-2 text-sm text-red-300 hover:bg-red-900/70 border border-red-700/40 transition-colors"
            >
              Delete Mod-list
            </button>
          }>
            <div class="flex items-center gap-2">
              <span class="max-w-md text-sm text-red-400">{deleteQuestion()}</span>
              <button
                onClick={() => void handleDelete()}
                disabled={deleting() || (!deletePreview() && !deletePreviewError()) || deletePreview()?.running === true}
                class="shrink-0 rounded-md bg-red-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-red-700 disabled:opacity-50"
              >
                {deleting() ? "Deleting..." : permanentPromised() ? "Yes, delete permanently" : "Yes, move to trash"}
              </button>
              <button
                onClick={cancelDelete}
                class="rounded-md bg-secondary px-3 py-1.5 text-sm text-secondary-foreground hover:bg-secondary/80"
              >
                Cancel
              </button>
            </div>
          </Show>
          <div class="flex gap-2 ml-auto">
            <button onClick={close} class="rounded-md bg-secondary px-4 py-2 text-sm text-secondary-foreground hover:bg-secondary/80">Cancel</button>
            <button onClick={() => void handleSave()} disabled={saving()} class="rounded-md bg-primary px-4 py-2 text-sm font-medium text-primary-foreground hover:bg-primary/90 disabled:opacity-50">{saving() ? "Saving..." : "Save Settings"}</button>
          </div>
        </div>
        </Show>
      </Modal>
    </Show>
  );
}
