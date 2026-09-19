import { For, Show, createEffect, createMemo, createSignal } from "solid-js";
import { modIcons, modNames } from "../../store";
import { contentProjects } from "../../lib/content-meta";
import { contentRowKey } from "../../lib/update-selection";
import { CONTENT_TAB_LABELS } from "../mod-list-editor/content-types";
import { AlertTriangleIcon, PackageIcon } from "../icons";
import { Modal, ModalHeader } from "./modal-base";
import type {
  ContentEntryWithoutVersions,
  ContentLookupFailure,
  ContentUpdateRow,
  ModUpdateRow,
} from "../../lib/types";

/**
 * The update popup, presentational on purpose: it receives the pre-check's
 * rows and reports the user's choice, and never talks to the backend.
 *
 * One window for mods and packs (D57), grouped by section rather than
 * sequenced through two modals: the launch starts once, and a second popup
 * would need a rule for every crossing of "accept mods / cancel packs" that
 * does not exist today.
 *
 * The two halves keep their own lookups. Mods resolve their icon and name
 * through `modIcons()`/`modNames()`, which the mod pipeline fills by project;
 * packs resolve theirs through the content-pack cache the editor rows already
 * use. Sharing one cache between them would key mod metadata by pack slug and
 * back.
 *
 * `onChoose` carries the accepted mod ids and the accepted
 * `category/entryId` keys — the two sets the launch turns into its two version
 * maps. "Skip" is that callback with both empty, so "skip" and "nothing
 * checked" are one code path instead of two that have to agree.
 */
export function UpdatePopup(props: {
  updates: ModUpdateRow[];
  contentUpdates: ContentUpdateRow[];
  contentWithoutVersions?: ContentEntryWithoutVersions[];
  contentLookupFailures?: ContentLookupFailure[];
  versionNumberLookupError?: string | null;
  onChoose: (accepted: ReadonlySet<string>, acceptedContent: ReadonlySet<string>) => void;
  onCancel: () => void;
}) {
  const [accepted, setAccepted] = createSignal<ReadonlySet<string>>(new Set());

  // Every row starts checked: the list is on screen and the action is
  // explicit, so the common case stays one click. Mod ids and
  // `category/entryId` keys live in one set — they cannot collide, a mod id
  // has no slash-prefixed category — and the two are split again on the way
  // out.
  const modKeys = createMemo(() => props.updates.map(row => row.modId));
  const contentKeys = createMemo(() => props.contentUpdates.map(contentRowKey));
  const allKeys = createMemo(() => [...modKeys(), ...contentKeys()]);
  createEffect(() => setAccepted(new Set(allKeys())));

  /** The categories with at least one row, in the order the tabs use. */
  const categories = createMemo(() => {
    const present = new Set(props.contentUpdates.map(row => row.category));
    return ["resourcepack", "shader", "datapack"].filter(category => present.has(category));
  });

  const allAccepted = () => allKeys().length > 0 && accepted().size === allKeys().length;
  const someAccepted = () => accepted().size > 0;
  const displayName = (row: ModUpdateRow) => modNames().get(row.projectId) ?? row.modId;
  const iconUrl = (row: ModUpdateRow) => modIcons().get(row.projectId);
  const contentName = (row: ContentUpdateRow) => contentProjects().get(row.entryId)?.name ?? row.entryId;
  const contentIcon = (row: ContentUpdateRow) => contentProjects().get(row.entryId)?.iconUrl;

  const toggleRow = (key: string, checked: boolean) => {
    const next = new Set(accepted());
    if (checked) next.add(key); else next.delete(key);
    setAccepted(next);
  };

  const choose = (keys: ReadonlySet<string>) => {
    const mods = new Set(modKeys().filter(key => keys.has(key)));
    const content = new Set(contentKeys().filter(key => keys.has(key)));
    props.onChoose(mods, content);
  };

  return (
    <Modal onClose={props.onCancel}>
      <ModalHeader
        title="Updates available"
        onClose={props.onCancel}
        actions={
          <label class="flex items-center gap-2 text-sm text-foreground">
            <input
              type="checkbox"
              checked={allAccepted()}
              ref={el => { createEffect(() => { el.indeterminate = someAccepted() && !allAccepted(); }); }}
              onChange={e => setAccepted(e.currentTarget.checked ? new Set(allKeys()) : new Set())}
              class="h-4 w-4 rounded text-primary"
            />
            <span>Select all</span>
          </label>
        }
      />

      {/* One non-blocking warning for the whole payload, not one per row (D24). */}
      <Show when={props.versionNumberLookupError}>
        <div class="flex items-start gap-2 border-b border-border bg-warning/10 px-6 py-3">
          <AlertTriangleIcon class="mt-0.5 h-4 w-4 shrink-0 text-warning" />
          <p class="text-sm text-warning">
            Current version numbers could not be loaded from Modrinth. The updates below are still accurate.
          </p>
        </div>
      </Show>

      {/* Neither is an update, and both are silence the user pays for: an
          entry with no version for this target (D60) and an entry nobody
          could ask about (D63). Different sentences on purpose. */}
      <Show when={(props.contentWithoutVersions?.length ?? 0) > 0}>
        <div class="flex items-start gap-2 border-b border-border bg-warning/10 px-6 py-3">
          <AlertTriangleIcon class="mt-0.5 h-4 w-4 shrink-0 text-warning" />
          <p class="text-sm text-warning">
            No version for this target:{" "}
            {props.contentWithoutVersions!.map(entry => entry.entryId).join(", ")}. They stay in the
            list and this launch installs nothing for them.
          </p>
        </div>
      </Show>
      <Show when={(props.contentLookupFailures?.length ?? 0) > 0}>
        <div class="flex items-start gap-2 border-b border-border bg-warning/10 px-6 py-3">
          <AlertTriangleIcon class="mt-0.5 h-4 w-4 shrink-0 text-warning" />
          <p class="text-sm text-warning">
            Modrinth could not be asked about{" "}
            {props.contentLookupFailures!.map(entry => entry.entryId).join(", ")}. Whether they have
            an update is unknown; the launch keeps what you have.
          </p>
        </div>
      </Show>

      <div class="flex-1 space-y-1 overflow-y-auto px-6 py-3">
        <Show when={props.updates.length > 0 && props.contentUpdates.length > 0}>
          <p class="px-2 pt-1 text-xs font-semibold uppercase tracking-wide text-muted-foreground">Mods</p>
        </Show>
        <For each={props.updates}>
          {row => (
            <label class="flex cursor-pointer items-center gap-3 rounded-md px-2 py-2 transition-colors hover:bg-muted/50">
              <div class="flex h-8 w-8 shrink-0 items-center justify-center overflow-hidden rounded-md bg-muted">
                <Show when={iconUrl(row)} fallback={<PackageIcon class="h-4 w-4 text-muted-foreground" />}>
                  <img
                    src={iconUrl(row)!}
                    alt={displayName(row)}
                    class="h-8 w-8 object-cover"
                    onError={e => { e.currentTarget.style.display = "none"; }}
                  />
                </Show>
              </div>

              <span class="min-w-0 flex-1 truncate text-sm font-medium text-foreground">{displayName(row)}</span>

              <span class="flex shrink-0 items-center gap-2 text-sm">
                {/* D24: a dash when the current version number is unknown; the arrow and the new version stay. */}
                <span class="text-muted-foreground">{row.currentVersionNumber ?? "—"}</span>
                <span class="text-muted-foreground">&rarr;</span>
                <span class="font-medium text-foreground">{row.candidateVersionNumber}</span>
              </span>

              <input
                type="checkbox"
                checked={accepted().has(row.modId)}
                onChange={e => toggleRow(row.modId, e.currentTarget.checked)}
                class="h-4 w-4 shrink-0 rounded text-primary"
              />
            </label>
          )}
        </For>

        <For each={categories()}>
          {category => (
            <>
              <p class="px-2 pt-3 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
                {CONTENT_TAB_LABELS[category] ?? category}
              </p>
              <For each={props.contentUpdates.filter(row => row.category === category)}>
                {row => (
                  <label class="flex cursor-pointer items-center gap-3 rounded-md px-2 py-2 transition-colors hover:bg-muted/50">
                    <div class="flex h-8 w-8 shrink-0 items-center justify-center overflow-hidden rounded-md bg-muted">
                      <Show when={contentIcon(row)} fallback={<PackageIcon class="h-4 w-4 text-muted-foreground" />}>
                        <img
                          src={contentIcon(row)!}
                          alt={contentName(row)}
                          class="h-8 w-8 object-cover"
                          onError={e => { e.currentTarget.style.display = "none"; }}
                        />
                      </Show>
                    </div>

                    <span class="min-w-0 flex-1 truncate text-sm font-medium text-foreground">{contentName(row)}</span>

                    <span class="flex shrink-0 items-center gap-2 text-sm">
                      <span class="text-muted-foreground">{row.currentVersionNumber}</span>
                      <span class="text-muted-foreground">&rarr;</span>
                      <span class="font-medium text-foreground">{row.candidateVersionNumber}</span>
                    </span>

                    <input
                      type="checkbox"
                      checked={accepted().has(contentRowKey(row))}
                      onChange={e => toggleRow(contentRowKey(row), e.currentTarget.checked)}
                      class="h-4 w-4 shrink-0 rounded text-primary"
                    />
                  </label>
                )}
              </For>
            </>
          )}
        </For>
      </div>

      <div class="flex justify-end gap-2 border-t border-border px-6 py-4">
        <button
          onClick={() => choose(new Set())}
          class="rounded-md bg-secondary px-4 py-2 text-sm text-secondary-foreground hover:bg-secondary/80"
        >
          Skip
        </button>
        <button
          onClick={() => choose(accepted())}
          class="rounded-md bg-primary px-4 py-2 text-sm font-medium text-white hover:bg-brandPurpleHover"
        >
          Update &amp; play
        </button>
      </div>
    </Modal>
  );
}
