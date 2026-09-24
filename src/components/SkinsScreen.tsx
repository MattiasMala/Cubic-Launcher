// The skin screen (E7), shaped like Modrinth's: the 3D preview large on the
// left, the library as a grid on the right, the capes below it.
//
// **Choosing is free, applying is a write.** Clicking a card or a cape only
// changes the preview; "Apply" is what reaches Mojang — one upload for the
// skin, one call for the cape, each only if it changed. Every Mojang write
// counts against a limit that is not measured (D86), so nothing here sends
// one without being asked. The cape stays a choice of its own (D88): wearing
// a skin never touches it.
//
// **The view is redrawn from what each command returns**, never patched from
// what it believed before: Mojang re-encodes an upload, so the card just
// applied can come back under another texture key (report 061). The
// selection is kept by identity when it survives, and falls back to the worn
// card when it does not.
//
// Errors are objects; the screen reads `kind`, never the message text, to
// decide what to say. A profile that can't be read is not an error of the
// screen: the library shows, and no card is marked as worn.

import { For, Show, createEffect, createMemo, createSignal, on, onCleanup } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { inferModelType, loadImage, loadSkinToCanvas } from "skinview-utils";
import { activeAccountId, setAccountsModalOpen } from "../store";
import type { SkinCard, SkinError, SkinVariant, SkinsView } from "../lib/types";
import { AlertTriangleIcon, CheckIcon, Loader2Icon, PencilIcon, PlusIcon, Trash2Icon, XIcon } from "./icons";
import { CapeFigure, SkinFigure } from "./skins/SkinFigures";
import { SkinPreview, type PreviewLoad } from "./skins/SkinPreview";

type CardId = Pick<SkinCard, "source" | "textureKey" | "variant">;

const idOf = (card: SkinCard): CardId => ({ source: card.source, textureKey: card.textureKey, variant: card.variant });

function asSkinError(error: unknown): SkinError {
  if (error && typeof error === "object" && "kind" in error) return error as SkinError;
  return { kind: "library", message: String(error), status: null, retryAfterSeconds: null };
}

function cardTitle(card: SkinCard) {
  if (card.name) return card.name;
  return card.source === "external" ? "Current skin" : "Unnamed skin";
}

function formatWait(seconds: number) {
  if (seconds < 60) return seconds === 1 ? "1 second" : `${seconds} seconds`;
  const minutes = Math.floor(seconds / 60);
  const rest = seconds % 60;
  return rest === 0 ? `${minutes} min` : `${minutes} min ${rest} s`;
}

/** What went wrong with an action, by kind. `wait` is the live countdown of a 429. */
function actionErrorText(error: SkinError, wait: number): { title: string; detail: string | null } {
  switch (error.kind) {
    case "rateLimited":
      return {
        title: wait > 0
          ? `Mojang limits how often a skin can change. Try again in ${formatWait(wait)}.`
          : "Mojang limits how often a skin can change. You can try again now.",
        detail: null,
      };
    case "notSignedIn":
      return { title: "Sign in again to change your skin.", detail: error.message };
    case "invalidSkin":
      return { title: "That isn't a skin Minecraft can use.", detail: error.message };
    case "network":
      return { title: "Couldn't reach Mojang. Check your connection and try again.", detail: null };
    case "mojang":
      return { title: "Mojang refused the change.", detail: error.message };
    case "library":
      return { title: "Your skin library couldn't be updated.", detail: error.message };
  }
}

function profileErrorText(error: SkinError) {
  switch (error.kind) {
    case "network":
      return "Mojang isn't answering, so the skin and capes on your profile can't be shown. Your library is still here.";
    case "notSignedIn":
      return "Your sign-in needs renewing before your profile can be read. Your library is still here.";
    case "rateLimited":
      return "Mojang asked to slow down before your profile can be read again. Your library is still here.";
    default:
      return "Mojang couldn't return your profile. Your library is still here.";
  }
}

/** The arms a picked file was drawn for, read the way the preview reads them. */
async function guessArms(path: string): Promise<"classic" | "slim"> {
  try {
    const url = await invoke<string>("read_image_as_data_url_command", { path });
    const canvas = document.createElement("canvas");
    loadSkinToCanvas(canvas, await loadImage(url));
    return inferModelType(canvas) === "slim" ? "slim" : "classic";
  } catch {
    // Not a skin at all: the backend refuses it with the reason.
    return "classic";
  }
}

const buttonBase =
  "inline-flex items-center justify-center gap-1.5 rounded-md px-3 py-1.5 text-sm font-medium transition-colors duration-75 disabled:opacity-50 disabled:cursor-not-allowed cursor-pointer";
const secondaryButton = `${buttonBase} bg-bgHover text-textMain hover:bg-[#333] border border-borderColor`;
const primaryButton = `${buttonBase} bg-primary text-white hover:bg-brandPurpleHover`;
const dangerButton = `${buttonBase} bg-destructive text-white hover:opacity-90`;

export function SkinsScreen() {
  const [view, setView] = createSignal<SkinsView | null>(null);
  /** The screen itself could not open: no account, or the library unreadable. */
  const [loadError, setLoadError] = createSignal<SkinError | null>(null);
  const [loading, setLoading] = createSignal(false);
  /** The operation running, if any: one at a time. */
  const [busy, setBusy] = createSignal<string | null>(null);
  const [actionError, setActionError] = createSignal<SkinError | null>(null);
  const [selectedId, setSelectedId] = createSignal<CardId | null>(null);
  /** A cape chosen in the preview and not yet applied; `{ id: null }` is "no cape". */
  const [pendingCape, setPendingCape] = createSignal<{ id: string | null } | null>(null);
  const [skinLoad, setSkinLoad] = createSignal<PreviewLoad>("loading");
  const [capeLoad, setCapeLoad] = createSignal<PreviewLoad>("ready");
  const [renaming, setRenaming] = createSignal(false);
  const [renameValue, setRenameValue] = createSignal("");
  const [confirmRemove, setConfirmRemove] = createSignal(false);

  // The 429 countdown: Mojang (or the local guard) said how long to wait.
  const [retryAt, setRetryAt] = createSignal<number | null>(null);
  const [now, setNow] = createSignal(Date.now());
  const waitSeconds = () => {
    const until = retryAt();
    return until === null ? 0 : Math.max(0, Math.ceil((until - now()) / 1000));
  };
  createEffect(() => {
    if (retryAt() === null) return;
    const timer = window.setInterval(() => {
      setNow(Date.now());
      if (waitSeconds() === 0) setRetryAt(null);
    }, 1000);
    onCleanup(() => window.clearInterval(timer));
  });

  const cards = () => view()?.skins ?? [];
  const capes = () => view()?.capes ?? [];
  const activeCard = () => cards().find(card => card.active) ?? null;
  const selectedCard = createMemo(() => {
    const id = selectedId();
    const found = id && cards().find(card =>
      card.source === id.source && card.textureKey === id.textureKey && card.variant === id.variant);
    return found || activeCard() || cards()[0] || null;
  });
  const ownCards = () => cards().filter(card => card.source !== "default");
  const defaultCards = () => cards().filter(card => card.source === "default");

  const activeCape = () => capes().find(cape => cape.active) ?? null;
  const previewCapeId = () => {
    const pending = pendingCape();
    return pending ? pending.id : activeCape()?.id ?? null;
  };
  const previewCape = () => capes().find(cape => cape.id === previewCapeId()) ?? null;

  const skinChanged = () => {
    const card = selectedCard();
    return card !== null && !card.active;
  };
  const capeChanged = () => {
    const pending = pendingCape();
    return pending !== null && pending.id !== (activeCape()?.id ?? null);
  };
  const previewing = () => skinChanged() || capeChanged();

  // A new selection starts with no half-finished rename or removal.
  createEffect(on(selectedCard, () => {
    setRenaming(false);
    setConfirmRemove(false);
  }));

  async function load() {
    setLoading(true);
    try {
      setView(await invoke<SkinsView>("load_skins_command"));
      setLoadError(null);
      setPendingCape(null);
    } catch (error) {
      setView(null);
      setLoadError(asSkinError(error));
    } finally {
      setLoading(false);
    }
  }
  createEffect(on(activeAccountId, () => {
    setSelectedId(null);
    void load();
  }));

  /**
   * Runs one command and redraws from its answer; `null` when it failed.
   * A failed write can still have changed the library — an equip keeps the
   * outgoing skin before the upload is refused — so a failure is followed by
   * a read of the screen as it is now (a read, not a write).
   */
  async function run(label: string, command: string, args: Record<string, unknown> = {}): Promise<SkinsView | null> {
    if (busy()) return null;
    setBusy(label);
    try {
      const next = await invoke<SkinsView>(command, args);
      setView(next);
      setActionError(null);
      return next;
    } catch (error) {
      const skinError = asSkinError(error);
      setActionError(skinError);
      if (skinError.kind === "rateLimited") {
        setNow(Date.now());
        setRetryAt(Date.now() + (skinError.retryAfterSeconds ?? 60) * 1000);
      }
      try {
        setView(await invoke<SkinsView>("load_skins_command"));
      } catch {
        // The screen keeps what it showed; the banner already says why.
      }
      return null;
    } finally {
      setBusy(null);
    }
  }

  async function apply() {
    const card = selectedCard();
    const cape = pendingCape();
    if (card && skinChanged()) {
      const next = await run("apply", "equip_skin_command", { textureKey: card.textureKey, variant: card.variant });
      if (!next) return;
      // The worn card, whatever key Mojang gave it.
      setSelectedId(null);
    }
    if (cape && capeChanged()) {
      const next = await run("apply", "set_cape_command", { capeId: cape.id });
      if (!next) return;
    }
    setPendingCape(null);
  }

  function discardPreview() {
    setSelectedId(null);
    setPendingCape(null);
  }

  async function addSkin() {
    const path = await open({
      title: "Choose a skin",
      filters: [{ name: "PNG images", extensions: ["png"] }],
      multiple: false,
      directory: false,
    });
    if (typeof path !== "string") return;
    const known = new Set(cards().filter(card => card.source === "saved").map(card => `${card.textureKey}/${card.variant}`));
    const next = await run("add", "add_skin_command", { path, variant: await guessArms(path), name: null });
    const added = next?.skins.find(card => card.source === "saved" && !known.has(`${card.textureKey}/${card.variant}`));
    if (added) setSelectedId(idOf(added));
  }

  async function setArms(card: SkinCard, variant: SkinVariant) {
    if (card.variant === variant) return;
    const next = await run("arms", "set_saved_skin_variant_command", {
      textureKey: card.textureKey,
      variant: card.variant,
      newVariant: variant,
    });
    if (next) setSelectedId({ source: "saved", textureKey: card.textureKey, variant });
  }

  async function rename(card: SkinCard) {
    const next = await run("rename", "rename_saved_skin_command", {
      textureKey: card.textureKey,
      variant: card.variant,
      name: renameValue(),
    });
    if (next) setRenaming(false);
  }

  async function remove(card: SkinCard) {
    const next = await run("remove", "remove_saved_skin_command", { textureKey: card.textureKey, variant: card.variant });
    if (next) setSelectedId(null);
  }

  async function saveWorn() {
    const next = await run("save", "save_worn_skin_command");
    if (next) setSelectedId(null);
  }

  async function resetToDefault() {
    const next = await run("reset", "reset_skin_command");
    if (next) setSelectedId(null);
  }

  const applyLabel = () => {
    if (busy() === "apply") return "Applying…";
    if (waitSeconds() > 0) return `Wait ${waitSeconds()} s`;
    return "Apply";
  };

  return (
    <div class="flex-1 min-h-0 overflow-y-auto bg-bgDark">
      <Show
        when={view()}
        fallback={
          <div class="px-6 py-5">
            <h1 class="text-xl font-semibold text-white">Skins</h1>
            <Show when={loading()}>
              <p class="mt-6 flex items-center gap-2 text-sm text-textMuted">
                <Loader2Icon class="h-4 w-4 animate-spin" /> Reading your skins…
              </p>
            </Show>
            <Show when={!loading() && loadError()}>
              {error => (
                <div class="mt-6 max-w-xl rounded-lg border border-borderColor bg-bgPanel p-5">
                  <p class="text-sm text-white">
                    {error().kind === "notSignedIn"
                      ? "Sign in with a Microsoft account to choose your skin."
                      : "The skin screen couldn't open."}
                  </p>
                  <p class="mt-1 text-xs text-textMuted">{error().message}</p>
                  <div class="mt-4 flex gap-2">
                    <Show when={error().kind === "notSignedIn"}>
                      <button type="button" class={primaryButton} onClick={() => setAccountsModalOpen(true)}>
                        Manage accounts
                      </button>
                    </Show>
                    <button type="button" class={secondaryButton} onClick={() => void load()}>
                      Try again
                    </button>
                  </div>
                </div>
              )}
            </Show>
          </div>
        }
      >
        {current => (
          <div class="grid grid-cols-[minmax(0,1fr)_minmax(0,2.5fr)] gap-10 px-6 py-5">
            {/* ── The preview ─────────────────────────────────────────── */}
            <div class="sticky top-5 self-start flex flex-col gap-4">
              <div class="flex items-baseline gap-3">
                <h1 class="text-xl font-semibold text-white">Skins</h1>
                <Show when={current().playerName}>
                  <span class="text-sm text-textMuted truncate">{current().playerName}</span>
                </Show>
                <Show when={busy() || loading()}>
                  <Loader2Icon class="h-4 w-4 animate-spin text-textMuted" />
                </Show>
              </div>

              <div class="relative h-[calc(80vh-9rem)] min-h-[320px] rounded-xl border border-borderColor bg-bgPanel overflow-hidden">
                <SkinPreview
                  skinUrl={selectedCard()?.textureUrl ?? null}
                  variant={selectedCard()?.variant ?? "classic"}
                  capeUrl={previewCape()?.textureUrl ?? null}
                  onSkinLoad={setSkinLoad}
                  onCapeLoad={setCapeLoad}
                />
                <Show when={previewing()}>
                  <span class="pointer-events-none absolute left-1/2 top-3 -translate-x-1/2 rounded-full border border-primary bg-primary/15 px-3 py-1 text-xs font-semibold text-white">
                    Previewing
                  </span>
                </Show>
                <Show when={skinLoad() === "failed"}>
                  <div class="pointer-events-none absolute inset-0 flex items-center justify-center p-6 text-center text-sm text-textMuted">
                    This skin is on Mojang's texture server, and it can't be reached right now.
                  </div>
                </Show>
                <Show when={capeLoad() === "failed" && skinLoad() !== "failed"}>
                  <p class="pointer-events-none absolute inset-x-0 bottom-3 text-center text-xs text-textMuted">
                    The cape can't be drawn without a connection.
                  </p>
                </Show>
              </div>

              <Show when={previewing()}>
                <div class="flex gap-2">
                  {/* With the profile unread nothing is worn, so there is nothing to go back to. */}
                  <Show when={activeCard() || capeChanged()}>
                    <button type="button" class={`${secondaryButton} flex-1`} disabled={!!busy()} onClick={discardPreview}>
                      <XIcon class="h-4 w-4" /> Back to worn
                    </button>
                  </Show>
                  <button
                    type="button"
                    class={`${primaryButton} flex-1`}
                    disabled={!!busy() || waitSeconds() > 0}
                    onClick={() => void apply()}
                  >
                    <Show when={busy() === "apply"} fallback={<CheckIcon class="h-4 w-4" />}>
                      <Loader2Icon class="h-4 w-4 animate-spin" />
                    </Show>
                    {applyLabel()}
                  </button>
                </div>
              </Show>

              <Show when={selectedCard()}>
                {card => (
                  <div class="rounded-lg border border-borderColor bg-bgPanel p-3 flex flex-col gap-3">
                    <Show
                      when={renaming()}
                      fallback={
                        <div class="min-w-0">
                          <p class="text-sm font-medium text-white truncate">{cardTitle(card())}</p>
                          <p class="text-xs text-textMuted">
                            {card().source === "saved" ? "In your skins" : card().source === "default" ? "Default skin" : "Worn, not in your skins yet"}
                            {" · "}
                            {card().variant === "slim" ? "Slim arms" : card().variant === "classic" ? "Classic arms" : "Unknown arms"}
                          </p>
                        </div>
                      }
                    >
                      <form
                        class="flex gap-2"
                        onSubmit={event => {
                          event.preventDefault();
                          void rename(card());
                        }}
                      >
                        <input
                          ref={element => queueMicrotask(() => element.focus())}
                          value={renameValue()}
                          onInput={event => setRenameValue(event.currentTarget.value)}
                          onKeyDown={event => event.key === "Escape" && setRenaming(false)}
                          maxLength={64}
                          placeholder="Name"
                          class="min-w-0 flex-1 rounded-md border border-borderColor bg-bgDark px-2 py-1 text-sm text-white outline-none focus:border-primary"
                        />
                        <button type="submit" class={primaryButton} disabled={!!busy()}>Save</button>
                        <button type="button" class={secondaryButton} onClick={() => setRenaming(false)}>Cancel</button>
                      </form>
                    </Show>

                    <Show when={card().source === "saved"}>
                      <div class="flex flex-wrap gap-2">
                        <div class="inline-flex rounded-md border border-borderColor overflow-hidden" role="group" aria-label="Arms">
                          <For each={["classic", "slim"] as const}>
                            {variant => (
                              <button
                                type="button"
                                disabled={!!busy()}
                                aria-pressed={card().variant === variant}
                                onClick={() => void setArms(card(), variant)}
                                class={`px-3 py-1.5 text-sm cursor-pointer disabled:cursor-not-allowed ${
                                  card().variant === variant ? "bg-primary text-white" : "bg-bgHover text-textMuted hover:text-white"
                                }`}
                              >
                                {variant === "classic" ? "Classic" : "Slim"}
                              </button>
                            )}
                          </For>
                        </div>
                        <button
                          type="button"
                          class={secondaryButton}
                          disabled={!!busy()}
                          onClick={() => {
                            setRenameValue(card().name ?? "");
                            setRenaming(true);
                          }}
                        >
                          <PencilIcon class="h-4 w-4" /> Rename
                        </button>
                        <Show
                          when={confirmRemove()}
                          fallback={
                            <button type="button" class={secondaryButton} disabled={!!busy()} onClick={() => setConfirmRemove(true)}>
                              <Trash2Icon class="h-4 w-4" /> Remove
                            </button>
                          }
                        >
                          <button type="button" class={dangerButton} disabled={!!busy()} onClick={() => void remove(card())}>
                            <Trash2Icon class="h-4 w-4" /> Remove for good
                          </button>
                          <button type="button" class={secondaryButton} onClick={() => setConfirmRemove(false)}>
                            Keep
                          </button>
                        </Show>
                      </div>
                    </Show>

                    <Show when={card().source === "external"}>
                      <button type="button" class={secondaryButton} disabled={!!busy()} onClick={() => void saveWorn()}>
                        <PlusIcon class="h-4 w-4" /> Save to your skins
                      </button>
                    </Show>
                  </div>
                )}
              </Show>

              <button
                type="button"
                class="self-start text-xs text-textMuted hover:text-white underline-offset-2 hover:underline cursor-pointer disabled:cursor-not-allowed disabled:opacity-50"
                disabled={!!busy() || waitSeconds() > 0}
                onClick={() => void resetToDefault()}
                title="Mojang puts one of its default skins on; the one you wear now is kept in your skins."
              >
                Reset to Mojang's default skin
              </button>
            </div>

            {/* ── The library and the capes ───────────────────────────── */}
            <div class="flex flex-col gap-8 min-w-0">
              <Show when={current().profileError}>
                {error => (
                  <div class="flex items-start gap-3 rounded-lg border border-warning/40 bg-warning/10 p-3">
                    <AlertTriangleIcon class="h-5 w-5 shrink-0 text-warning" />
                    <div class="min-w-0 flex-1">
                      <p class="text-sm text-white">{profileErrorText(error())}</p>
                      <p class="mt-0.5 text-xs text-textMuted">Nothing is marked as worn until Mojang answers.</p>
                    </div>
                    <Show when={error().kind === "notSignedIn"}>
                      <button type="button" class={secondaryButton} onClick={() => setAccountsModalOpen(true)}>
                        Manage accounts
                      </button>
                    </Show>
                    <button type="button" class={secondaryButton} disabled={loading()} onClick={() => void load()}>
                      Try again
                    </button>
                  </div>
                )}
              </Show>

              <Show when={actionError()}>
                {error => (
                  <div class="flex items-start gap-3 rounded-lg border border-destructive/40 bg-destructive/10 p-3">
                    <AlertTriangleIcon class="h-5 w-5 shrink-0 text-destructive" />
                    <div class="min-w-0 flex-1">
                      <p class="text-sm text-white">{actionErrorText(error(), waitSeconds()).title}</p>
                      <Show when={actionErrorText(error(), waitSeconds()).detail}>
                        {detail => <p class="mt-0.5 text-xs text-textMuted break-words">{detail()}</p>}
                      </Show>
                    </div>
                    <Show when={error().kind === "notSignedIn"}>
                      <button type="button" class={secondaryButton} onClick={() => setAccountsModalOpen(true)}>
                        Manage accounts
                      </button>
                    </Show>
                    <button
                      type="button"
                      class="w-8 h-8 shrink-0 rounded-md text-textMuted hover:text-white hover:bg-bgHover flex items-center justify-center cursor-pointer"
                      aria-label="Dismiss"
                      onClick={() => setActionError(null)}
                    >
                      <XIcon class="h-4 w-4" />
                    </button>
                  </div>
                )}
              </Show>

              <section>
                <h2 class="text-xs uppercase font-semibold tracking-wider text-textMuted mb-3">Your skins</h2>
                <div class="grid grid-cols-[repeat(auto-fill,minmax(104px,1fr))] gap-3">
                  <button
                    type="button"
                    disabled={!!busy()}
                    onClick={() => void addSkin()}
                    class="flex aspect-[3/4] flex-col items-center justify-center gap-2 rounded-lg border border-dashed border-borderColor text-textMuted hover:border-primary hover:text-white cursor-pointer disabled:cursor-not-allowed disabled:opacity-50"
                  >
                    <PlusIcon class="h-6 w-6" />
                    <span class="text-xs font-medium">Add skin</span>
                  </button>
                  <For each={ownCards()}>
                    {card => <SkinTile card={card} selected={selectedCard() === card} onSelect={() => setSelectedId(idOf(card))} />}
                  </For>
                </div>
                <Show when={ownCards().length === 0}>
                  <p class="mt-3 text-xs text-textMuted">
                    Skins you add, and the ones you wear before changing, are kept here.
                  </p>
                </Show>
              </section>

              <section>
                <h2 class="text-xs uppercase font-semibold tracking-wider text-textMuted mb-3">Default skins</h2>
                <div class="grid grid-cols-[repeat(auto-fill,minmax(104px,1fr))] gap-3">
                  <For each={defaultCards()}>
                    {card => <SkinTile card={card} selected={selectedCard() === card} onSelect={() => setSelectedId(idOf(card))} />}
                  </For>
                </div>
              </section>

              <section>
                <h2 class="text-xs uppercase font-semibold tracking-wider text-textMuted mb-3">Capes</h2>
                <Show
                  when={capes().length > 0}
                  fallback={
                    <p class="text-xs text-textMuted">
                      {current().profileError
                        ? "Your capes show up when Mojang answers."
                        : "This account owns no capes."}
                    </p>
                  }
                >
                  <div class="grid grid-cols-[repeat(auto-fill,minmax(104px,1fr))] gap-3">
                    <CapeTile
                      title="No cape"
                      worn={activeCape() === null}
                      selected={previewCapeId() === null}
                      onSelect={() => setPendingCape(activeCape() === null ? null : { id: null })}
                    />
                    <For each={capes()}>
                      {cape => (
                        <CapeTile
                          title={cape.alias ?? "Cape"}
                          url={cape.textureUrl}
                          worn={cape.active}
                          selected={previewCapeId() === cape.id}
                          onSelect={() => setPendingCape(cape.active ? null : { id: cape.id })}
                        />
                      )}
                    </For>
                  </div>
                </Show>
              </section>
            </div>
          </div>
        )}
      </Show>
    </div>
  );
}

function WornBadge() {
  return (
    <span class="absolute left-1.5 top-1.5 rounded bg-success/90 px-1.5 py-0.5 text-[10px] font-semibold text-white">
      Worn
    </span>
  );
}

function SkinTile(props: { card: SkinCard; selected: boolean; onSelect: () => void }) {
  return (
    <button
      type="button"
      aria-pressed={props.selected}
      onClick={() => props.onSelect()}
      class={`relative flex aspect-[3/4] flex-col items-center rounded-lg border bg-bgPanel p-2 cursor-pointer transition-colors duration-75 ${
        props.selected ? "border-primary ring-2 ring-primary/60" : "border-borderColor hover:border-textMuted"
      }`}
    >
      <Show when={props.card.active}>
        <WornBadge />
      </Show>
      <Show when={props.card.source === "external"}>
        <span class="absolute right-1.5 top-1.5 rounded bg-bgHover px-1.5 py-0.5 text-[10px] text-textMuted">Not saved</span>
      </Show>
      <SkinFigure url={props.card.textureUrl} slim={props.card.variant === "slim"} class="mt-3 w-full flex-1 min-h-0" />
      <span class="mt-1.5 w-full truncate text-center text-xs text-white">{cardTitle(props.card)}</span>
      <span class="w-full truncate text-center text-[10px] text-textMuted">{props.card.variant === "slim" ? "Slim" : "Classic"}</span>
    </button>
  );
}

function CapeTile(props: { title: string; url?: string; worn: boolean; selected: boolean; onSelect: () => void }) {
  return (
    <button
      type="button"
      aria-pressed={props.selected}
      onClick={() => props.onSelect()}
      class={`relative flex aspect-[3/4] flex-col items-center rounded-lg border bg-bgPanel p-2 cursor-pointer transition-colors duration-75 ${
        props.selected ? "border-primary ring-2 ring-primary/60" : "border-borderColor hover:border-textMuted"
      }`}
    >
      <Show when={props.worn}>
        <WornBadge />
      </Show>
      <Show
        when={props.url}
        fallback={
          <div class="mt-3 flex w-full flex-1 items-center justify-center text-textMuted">
            <XIcon class="h-8 w-8" />
          </div>
        }
      >
        {url => <CapeFigure url={url()} class="mt-5 w-full flex-1 min-h-0" />}
      </Show>
      <span class="mt-1.5 w-full truncate text-center text-xs text-white">{props.title}</span>
    </button>
  );
}
