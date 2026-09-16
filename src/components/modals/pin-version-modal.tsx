/**
 * PinVersionModal — pick the release a mod is frozen on (task B, D3/D5).
 *
 * Presentational on purpose: it never calls `invoke`. Everything that touches
 * the backend is passed in by `PinVersionDialog`, which is what makes this
 * modal mountable with fixtures for a screenshot.
 *
 * Both dropdowns are the in-DOM `Select` (commit `37fec99`): a native
 * `<select>` reintroduces the Wayland/WebKitGTK bug that component exists to
 * fix.
 */
import { Show, createEffect, createSignal } from "solid-js";

import { Modal, ModalHeader } from "./modal-base";
import { Select } from "../Select";
import { MaterialIcon } from "../icons";
import { type PinRelease } from "../../lib/pin-versions";

export interface PinVersionModalProps {
  /** Mod being pinned, as shown in the row. */
  modName: string;
  mcVersions: string[];
  initialMcVersion: string;
  /** The loader the pin is scoped to, shown as read-only context. */
  loader: string;
  /** Releases for the chosen Minecraft version; re-run on every change. */
  loadReleases: (mcVersion: string) => Promise<PinRelease[]>;
  /** The version currently pinned, when there is one: enables "Remove pin". */
  pinnedLabel?: string;
  /** False when the pin has no dynamic entry left to re-pin (D5 removed it). */
  canPin: boolean;
  onSave: (choice: { versionId: string; mcVersion: string; removeDynamic: boolean }) => Promise<void>;
  onRemovePin?: () => Promise<void>;
  onClose: () => void;
}

export function PinVersionModal(props: PinVersionModalProps) {
  const [mcVersion, setMcVersion] = createSignal(props.initialMcVersion);
  const [releases, setReleases] = createSignal<PinRelease[]>([]);
  const [versionId, setVersionId] = createSignal("");
  const [removeDynamic, setRemoveDynamic] = createSignal(false);
  const [loading, setLoading] = createSignal(false);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  // One effect for the whole dropdown-2 contents: it re-runs on the Minecraft
  // version, which is the only thing the release list depends on.
  createEffect(() => {
    const target = mcVersion();
    setLoading(true);
    setError(null);
    props
      .loadReleases(target)
      .then(list => {
        setReleases(list);
        setVersionId(list[0]?.id ?? "");
      })
      .catch(failure => {
        setReleases([]);
        setVersionId("");
        setError(`Could not list the releases for ${target}: ${String(failure)}`);
      })
      .finally(() => setLoading(false));
  });

  // "0.6.0 (release)", the label of the mock in todo.md § B2.
  const releaseOptions = () =>
    releases().map(release => ({
      value: release.id,
      label: `${release.versionNumber} (${release.versionType})`,
    }));

  const run = async (action: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await action();
    } catch (failure) {
      setError(String(failure));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal onClose={props.onClose} maxWidth="max-w-lg">
      <ModalHeader
        title={`${props.modName} — choose version`}
        description={`The pin applies to ${props.loader} only; every other target keeps using the dynamic entry.`}
        onClose={props.onClose}
      />

      <div class="flex flex-col gap-4 px-6 py-5">
        <Show when={props.pinnedLabel}>
          <div class="flex items-center gap-2 rounded-md border border-border bg-muted/40 px-3 py-2 text-xs text-muted-foreground">
            <MaterialIcon name="push_pin" size="sm" />
            <span>
              Currently pinned to <span class="text-foreground">{props.pinnedLabel}</span>. Saving replaces it.
            </span>
          </div>
        </Show>

        <div class="flex items-center justify-between gap-4">
          <span class="text-sm text-foreground">Minecraft version</span>
          <Select
            value={mcVersion()}
            options={props.mcVersions.map(version => ({ value: version, label: version }))}
            onChange={setMcVersion}
            disabled={busy()}
            class="min-w-[10rem] rounded border border-border bg-input px-2 py-1 text-sm text-foreground"
          />
        </div>

        <div class="flex items-center justify-between gap-4">
          <span class="text-sm text-foreground">Release</span>
          <Select
            value={versionId()}
            options={releaseOptions()}
            onChange={setVersionId}
            disabled={busy() || loading() || releaseOptions().length === 0}
            placeholder={loading() ? "Loading…" : "No release for this target"}
            class="min-w-[14rem] rounded border border-border bg-input px-2 py-1 text-sm text-foreground"
          />
        </div>

        <Show when={error()}>
          <p class="text-xs text-destructive">{error()}</p>
        </Show>
      </div>

      <div class="flex flex-col items-end gap-2 border-t border-border px-6 py-4">
        <div class="flex w-full items-center justify-between gap-2">
          <Show when={props.onRemovePin} fallback={<span />}>
            <button
              onClick={() => void run(async () => { await props.onRemovePin!(); })}
              disabled={busy()}
              class="flex items-center gap-1.5 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm font-medium text-destructive transition-colors hover:bg-destructive/20 disabled:opacity-40"
            >
              <MaterialIcon name="link_off" size="sm" />
              Remove pin
            </button>
          </Show>
          <button
            onClick={() =>
              void run(async () => {
                await props.onSave({ versionId: versionId(), mcVersion: mcVersion(), removeDynamic: removeDynamic() });
              })
            }
            disabled={busy() || loading() || !props.canPin || versionId() === ""}
            class="rounded-md bg-primary px-4 py-2 text-sm font-medium text-white transition-colors hover:bg-brandPurpleHover disabled:opacity-40 disabled:cursor-not-allowed"
            title={props.canPin ? "Pin this release" : "This pin has no dynamic entry left to re-pin"}
          >
            {busy() ? "Saving…" : "Save"}
          </button>
        </div>
        <label class="flex items-center gap-3 text-sm">
          <input
            type="checkbox"
            checked={removeDynamic()}
            disabled={busy() || !props.canPin}
            onChange={event => setRemoveDynamic(event.currentTarget.checked)}
            class="h-4 w-4 rounded text-primary"
          />
          <span class="text-muted-foreground">Remove the dynamic entry</span>
        </label>
      </div>
    </Modal>
  );
}
