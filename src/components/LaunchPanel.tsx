import { For, Show, createSignal, createEffect } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { MOD_LOADERS, type AccountSummary } from "../lib/types";
import {
  selectedMcVersion, setSelectedMcVersion,
  selectedModLoader, setSelectedModLoader,
  minecraftVersions, mcWithSnapshots, showSnapshots, setShowSnapshots,
  launchState, launchProgress, activeLaunchStage, updateCheckRunning,
  launchLogs, logViewerOpen, setLogViewerOpen,
  activeAccount, accounts, setAccountsModalOpen,
} from "../store";
import { MaterialIcon, Loader2Icon, XIcon } from "./icons";
import { Select } from "./Select";

interface LaunchPanelProps {
  onLaunch: () => void;
  onSwitchAccount: (id: string) => Promise<void>;
  onVersionChange?: (version: string) => void;
  onLoaderChange?: (loader: string) => void;
}

// What the account under the Play button can actually do (F1). Before this it
// said "Microsoft Account" even when every launch fell back to offline.
function accountSubtitle(account: AccountSummary | undefined): { text: string; warn: boolean } {
  switch (account?.credentials) {
    case "unreadable":
    case "signed_out":
      return { text: "Offline · sign in again", warn: true };
    case "keyring_unavailable":
      return { text: "Offline · keyring unavailable", warn: true };
    case "usable":
      return { text: "Microsoft Account", warn: false };
    default:
      // Not described by the backend (no account at all, before the snapshot).
      return account?.status === "online"
        ? { text: "Microsoft Account", warn: false }
        : { text: "Offline", warn: true };
  }
}

export function LaunchPanel(props: LaunchPanelProps) {
  const isLaunching = () => launchState() === "resolving" || launchState() === "running";
  const isRunning = () => launchState() === "running";
  const [accountMenuOpen, setAccountMenuOpen] = createSignal(false);
  const [autoScroll, setAutoScroll] = createSignal(true);
  const [showErrors, setShowErrors] = createSignal(true);
  const [showWarnings, setShowWarnings] = createSignal(true);
  const [showInfo, setShowInfo] = createSignal(true);
  const [showGame, setShowGame] = createSignal(true);
  let logContainer: HTMLDivElement | undefined;

  const logLevel = (line: string): "error" | "warning" | "info" | "game" => {
    const lower = line.toLowerCase();
    if (lower.includes("error") || lower.includes("✗") || lower.includes("exception") || lower.includes("fatal") || line.startsWith("[Launch]") && lower.includes("fail")) return "error";
    if (lower.includes("warn")) return "warning";
    if (line.startsWith("[Launcher]") || line.startsWith("[Resolver]") || line.startsWith("[Java]") || line.startsWith("[Cache]") || line.startsWith("[Launch]") || line.startsWith("[Cubic]")) return "info";
    return "game";
  };

  const filteredLogs = () => launchLogs().filter(line => {
    const level = logLevel(line);
    if (level === "error") return showErrors();
    if (level === "warning") return showWarnings();
    if (level === "info") return showInfo();
    return showGame();
  });

  createEffect(() => {
    filteredLogs(); // track changes
    if (autoScroll() && logContainer) {
      logContainer.scrollTop = logContainer.scrollHeight;
    }
  });

  const handleStop = async () => {
    try {
      await invoke("stop_minecraft_command");
    } catch { /* already exited */ }
  };

  return (
    <div class="shrink-0 border-t border-borderColor bg-bgPanel shadow-glow z-10">

      {/* ── Log viewer ─────────────────────────────────────────────── */}
      <Show when={logViewerOpen()}>
        <div class="border-b border-borderColor">
          <div class="flex items-center justify-between bg-bgHover px-4 py-2">
            <div class="flex items-center gap-2 text-sm font-medium text-textMain">
              <MaterialIcon name="terminal" size="md" />
              Launch Log
            </div>
            <div class="flex items-center gap-2">
              <label class="flex items-center gap-1 cursor-pointer select-none">
                <input type="checkbox" checked={autoScroll()} onChange={e => setAutoScroll(e.currentTarget.checked)} class="accent-accentColor w-3 h-3" />
                <span class="text-xs text-textMuted">Auto-scroll</span>
              </label>
              <button
                onClick={() => { if (logContainer) logContainer.scrollTop = logContainer.scrollHeight; }}
                class="flex h-6 w-6 items-center justify-center rounded-md text-textMuted transition-colors hover:bg-bgDark hover:text-white"
                title="Scroll to bottom"
              >
                <MaterialIcon name="vertical_align_bottom" size="sm" />
              </button>
              <button onClick={() => setLogViewerOpen(false)} class="flex h-6 w-6 items-center justify-center rounded-md text-textMuted transition-colors hover:bg-bgDark hover:text-white">
                <XIcon class="h-4 w-4" />
              </button>
            </div>
          </div>
          {/* Filter bar */}
          <div class="flex items-center gap-2 bg-bgDark/80 px-4 py-1.5 border-b border-borderColor/30">
            <span class="text-[10px] text-textMuted mr-1">Filter:</span>
            <button
              onClick={() => setShowErrors(v => !v)}
              class={`rounded px-2 py-0.5 text-[10px] font-medium transition-colors ${showErrors() ? "bg-red-500/20 text-red-400 ring-1 ring-red-500/30" : "text-textMuted/50 line-through"}`}
            >
              Errors
            </button>
            <button
              onClick={() => setShowWarnings(v => !v)}
              class={`rounded px-2 py-0.5 text-[10px] font-medium transition-colors ${showWarnings() ? "bg-amber-500/20 text-amber-400 ring-1 ring-amber-500/30" : "text-textMuted/50 line-through"}`}
            >
              Warnings
            </button>
            <button
              onClick={() => setShowInfo(v => !v)}
              class={`rounded px-2 py-0.5 text-[10px] font-medium transition-colors ${showInfo() ? "bg-primary/20 text-primary ring-1 ring-primary/30" : "text-textMuted/50 line-through"}`}
            >
              Info
            </button>
            <button
              onClick={() => setShowGame(v => !v)}
              class={`rounded px-2 py-0.5 text-[10px] font-medium transition-colors ${showGame() ? "bg-muted text-textMuted ring-1 ring-borderColor" : "text-textMuted/50 line-through"}`}
            >
              Game
            </button>
          </div>
          <div ref={logContainer} class="h-48 overflow-y-auto bg-bgDark px-3 py-2 font-mono text-xs">
            <Show
              when={filteredLogs().length > 0}
              fallback={<p class="text-textMuted">Launch logs will appear here...</p>}
            >
              <For each={filteredLogs()}>
                {(line, i) => {
                  const level = logLevel(line);
                  return (
                    <div class={`flex gap-3 border-b border-borderColor/20 py-1 last:border-b-0 ${
                      level === "error" ? "text-red-400" :
                      level === "warning" ? "text-amber-400" :
                      level === "info" ? "text-primary" :
                      "text-textMuted"
                    }`}>
                      <span class="shrink-0 select-none text-primary/50">{String(i() + 1).padStart(2, "0")}</span>
                      <span class="break-all">{line || "\u00A0"}</span>
                    </div>
                  );
                }}
              </For>
            </Show>
          </div>
        </div>
      </Show>

      {/* ── Progress bar ───────────────────────────────────────────── */}
      <Show when={isLaunching()}>
        <div class="px-6 py-2">
          <div class="mb-1 flex items-center justify-between text-xs text-textMuted">
             <span>
               {launchState() === "resolving" ? "Resolving mods..." :
               launchState() === "running"   ? "Minecraft running..." :
                launchState() === "ready"     ? "Launch ready"   :
                                                "Idle"}
             </span>
            <span>{activeLaunchStage().label}</span>
          </div>
          <div class="h-1 overflow-hidden rounded-full bg-bgHover">
            <div
              class="h-full rounded-full bg-primary transition-all duration-300"
              style={{ width: `${launchProgress()}%` }}
            />
          </div>
        </div>
      </Show>

      {/* ── Main BottomBar layout ────────────────────────────────── */}
      <div class="h-24 px-6 flex items-center justify-between">

        {/* Left side: Account + Loader info */}
        <div class="flex items-center gap-6">
          {/* Account selector */}
          <div class="relative">
            <button
              onClick={() => setAccountMenuOpen(v => !v)}
              class="flex items-center gap-3 cursor-pointer hover:bg-bgHover p-2 rounded-lg transition-colors duration-75 border border-transparent hover:border-borderColor"
            >
              <Show when={activeAccount()?.avatarUrl} fallback={
                <div class="w-10 h-10 rounded-full border border-borderColor bg-primary/20 flex items-center justify-center text-primary font-bold">
                  {(activeAccount()?.gamertag ?? "?").slice(0, 2).toUpperCase()}
                </div>
              }>
                <img src={activeAccount()!.avatarUrl} alt="" class="w-10 h-10 rounded-md border border-borderColor" loading="lazy" />
              </Show>
              <div class="flex flex-col">
                <span class="text-sm font-semibold text-white">
                  {activeAccount()?.gamertag ?? "Not logged in"}
                </span>
                <span class={`text-xs flex items-center gap-1 ${accountSubtitle(activeAccount()).warn ? "text-warning" : "text-textMuted"}`}>
                  {accountSubtitle(activeAccount()).text}
                  <MaterialIcon name="expand_more" size="sm" />
                </span>
              </div>
            </button>

            {/* Account dropdown menu */}
            <Show when={accountMenuOpen()}>
              <div class="absolute bottom-full left-0 mb-2 w-64 rounded-lg border border-borderColor bg-bgPanel py-2 shadow-lg z-50">
                <For each={accounts()}>
                  {(account) => (
                    <button
                      onClick={() => {
                        void props.onSwitchAccount(account.id);
                        setAccountMenuOpen(false);
                      }}
                      class={`flex w-full items-center gap-3 px-4 py-2 text-sm transition-colors hover:bg-bgHover ${
                        account.id === activeAccount()?.id ? "text-primary" : "text-textMain"
                      }`}
                    >
                      <Show when={account.avatarUrl} fallback={
                        <div class="w-8 h-8 rounded-full bg-primary/20 flex items-center justify-center text-primary font-bold text-xs">
                          {account.gamertag.slice(0, 2).toUpperCase()}
                        </div>
                      }>
                        <img src={account.avatarUrl} alt="" class="w-8 h-8 rounded-md" loading="lazy" />
                      </Show>
                      <span class="flex-1 text-left truncate">{account.gamertag}</span>
                      <Show when={account.id === activeAccount()?.id}>
                        <MaterialIcon name="check" size="sm" class="text-primary" />
                      </Show>
                    </button>
                  )}
                </For>
                <div class="my-1 border-t border-borderColor" />
                <button
                  onClick={() => { setAccountsModalOpen(true); setAccountMenuOpen(false); }}
                  class="flex w-full items-center gap-3 px-4 py-2 text-sm text-textMain transition-colors hover:bg-bgHover"
                >
                  <MaterialIcon name="add" size="md" />
                  <span>Manage Accounts</span>
                </button>
              </div>
            </Show>
          </div>

          {/* Divider */}
          <div class="h-8 w-px bg-borderColor hidden md:block" />

          {/* Loader + Version info */}
          <div class="hidden md:flex flex-col">
            <div class="flex items-center gap-2 text-sm text-textMain font-medium">
              <MaterialIcon name="extension" size="sm" class="opacity-70" />
              <Select
                value={selectedModLoader()}
                options={(MOD_LOADERS as unknown as string[]).map(l => ({ value: l, label: `${l} Loader` }))}
                onChange={value => { props.onLoaderChange ? props.onLoaderChange(value) : setSelectedModLoader(value); }}
                class="text-sm text-textMain font-medium"
                direction="up"
              />
            </div>
            <div class="flex items-center gap-1">
              <span class="text-xs text-textMuted">Minecraft</span>
              <Select
                value={selectedMcVersion()}
                options={(showSnapshots() ? mcWithSnapshots() : minecraftVersions()).map(v => ({ value: v, label: v }))}
                onChange={value => { props.onVersionChange ? props.onVersionChange(value) : setSelectedMcVersion(value); }}
                class="text-xs text-textMuted"
                direction="up"
              />
            </div>
            <label class="flex items-center gap-1 cursor-pointer select-none">
              <input
                type="checkbox"
                checked={showSnapshots()}
                onChange={e => setShowSnapshots(e.currentTarget.checked)}
                class="accent-accentColor w-3 h-3"
              />
              <span class="text-xs text-textMuted">Show Snapshots</span>
            </label>
          </div>
        </div>

        {/* Right side: Log button + Play button */}
        <div class="flex items-center gap-4">
          {/* Log toggle */}
          <button
            onClick={() => setLogViewerOpen(v => !v)}
            class={`w-10 h-10 rounded-full bg-bgDark border border-borderColor flex items-center justify-center transition-colors duration-75 ${
              logViewerOpen()
                ? "text-primary border-primary"
                : "text-textMuted hover:text-white hover:bg-bgHover"
            }`}
            title="Logs"
          >
            <MaterialIcon name="terminal" size="md" />
          </button>

          {/* Play / Stop button */}
          <Show
            when={isRunning()}
            fallback={
              <button
                onClick={props.onLaunch}
                disabled={launchState() === "resolving" || updateCheckRunning()}
                class="px-10 py-3 bg-primary hover:bg-brandPurpleHover text-white font-bold text-lg rounded-lg shadow-lg flex items-center gap-3 transition-colors duration-75 disabled:opacity-70 disabled:cursor-not-allowed"
              >
                <Show
                  when={updateCheckRunning()}
                  fallback={
                    <Show
                      when={launchState() === "resolving"}
                      fallback={
                        <>
                          <MaterialIcon name="play_arrow" size="lg" />
                          PLAY
                        </>
                      }
                    >
                      <Loader2Icon class="h-6 w-6 animate-spin" />
                      {launchProgress()}%
                    </Show>
                  }
                >
                  <Loader2Icon class="h-6 w-6 animate-spin" />
                  CHECKING
                </Show>
              </button>
            }
          >
            <button
              onClick={() => void handleStop()}
              class="px-10 py-3 bg-red-600 hover:bg-red-700 text-white font-bold text-lg rounded-lg shadow-lg flex items-center gap-3 transition-colors duration-75"
            >
              <MaterialIcon name="stop" size="lg" />
              STOP
            </button>
          </Show>
        </div>
      </div>
    </div>
  );
}
