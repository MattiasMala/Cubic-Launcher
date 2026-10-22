import { For, Show } from "solid-js";
import { dismissAllUiErrors, dismissUiError, launcherErrors } from "../store";
import { AlertTriangleIcon, XIcon } from "./icons";

/**
 * The non-blocking notices of a launch, under the header (D28).
 *
 * `pushUiError` had no visible surface: a warning ended up in the launch log,
 * which is consultable and not visible. Everything the pipeline can say
 * without stopping the game lands here — a pre-check that failed (D19), a
 * cached jar restored, and above all a mod left out of the launch, which is
 * the one case where something is actually missing from the game.
 *
 * Same shape as `UpdateBanner`: an `aside` between the header and the
 * content, dismissible, never modal. Three at a time, because a Modrinth that
 * does not answer can produce one notice per mod, and a banner as tall as the
 * window is a modal by other means.
 */
const VISIBLE_NOTICES = 3;

export function NoticeBanner() {
  const hiddenCount = () => Math.max(launcherErrors().length - VISIBLE_NOTICES, 0);

  return (
    <Show when={launcherErrors().length > 0}>
      <aside class="shrink-0 border-b border-borderColor bg-bgPanel px-4 py-3" role="status">
        <div class="space-y-2">
          <For each={launcherErrors().slice(0, VISIBLE_NOTICES)}>
            {notice => (
              <div class="flex items-start gap-3">
                <AlertTriangleIcon
                  class={`mt-0.5 h-4 w-4 shrink-0 ${notice.severity === "error" ? "text-destructive" : "text-primary"}`}
                />
                <div class="min-w-0 flex-1">
                  <p class="text-sm font-semibold text-textMain">{notice.title}</p>
                  <p class="mt-0.5 text-xs text-textMuted">{notice.message}</p>
                  <Show when={notice.detail}>
                    <p class="mt-0.5 line-clamp-2 text-xs text-textMuted/80">{notice.detail}</p>
                  </Show>
                  <Show when={notice.action}>
                    {action => (
                      <button
                        type="button"
                        onClick={() => action().run()}
                        class="mt-1 text-xs font-medium text-primary underline underline-offset-2 transition-colors hover:text-white"
                      >
                        {action().label}
                      </button>
                    )}
                  </Show>
                </div>
                <button
                  type="button"
                  onClick={() => dismissUiError(notice.id)}
                  class="flex h-8 w-8 shrink-0 items-center justify-center rounded-md text-textMuted transition-colors hover:bg-white/10 hover:text-white"
                  aria-label={`Dismiss notice: ${notice.title}`}
                  title="Dismiss"
                >
                  <XIcon class="h-4 w-4" />
                </button>
              </div>
            )}
          </For>
          <div class="flex items-center gap-3 pl-7">
            <Show when={hiddenCount() > 0}>
              <p class="text-xs text-textMuted">{hiddenCount()} more notice(s)</p>
            </Show>
            <Show when={launcherErrors().length > 1}>
              <button
                type="button"
                onClick={dismissAllUiErrors}
                class="text-xs text-textMuted underline underline-offset-2 transition-colors hover:text-white"
              >
                Dismiss all
              </button>
            </Show>
          </div>
        </div>
      </aside>
    </Show>
  );
}
