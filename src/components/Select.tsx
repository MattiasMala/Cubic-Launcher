import { For, Show, createSignal, onCleanup } from "solid-js";
import { MaterialIcon } from "./icons";

export interface SelectOption {
  value: string;
  label: string;
}

export interface SelectProps {
  value: string;
  options: SelectOption[];
  onChange: (value: string) => void;
  /** Classes applied to the trigger button (styling comes from the call site). */
  class?: string;
  /** Classes applied to the popup panel in addition to the base ones. */
  panelClass?: string;
  /**
   * Which edge the popup is anchored to. Inside a modal a left-anchored panel
   * whose options are wider than the trigger runs past the right border and
   * gets clipped; anchoring it right makes it grow inwards instead.
   */
  align?: "left" | "right";
  /** Open the panel above the trigger (for controls near the bottom edge). */
  direction?: "up" | "down";
  disabled?: boolean;
  /** Label shown when no option matches `value`. */
  placeholder?: string;
  title?: string;
}

/**
 * In-DOM replacement for native `<select>`.
 *
 * WebKitGTK renders native selects as GTK popup windows; under Wayland the
 * popup loses its grab and closes immediately (works on Windows/X11 only).
 * Rendering the options inside the webview DOM sidesteps the GTK popup path
 * entirely, so this must be used instead of `<select>` everywhere.
 */
export function Select(props: SelectProps) {
  const [open, setOpen] = createSignal(false);
  let root: HTMLDivElement | undefined;

  const onDocumentPointerDown = (event: PointerEvent) => {
    if (root && !root.contains(event.target as Node)) setOpen(false);
  };
  const onDocumentKeyDown = (event: KeyboardEvent) => {
    if (event.key === "Escape") setOpen(false);
  };
  document.addEventListener("pointerdown", onDocumentPointerDown);
  document.addEventListener("keydown", onDocumentKeyDown);
  onCleanup(() => {
    document.removeEventListener("pointerdown", onDocumentPointerDown);
    document.removeEventListener("keydown", onDocumentKeyDown);
  });

  return (
    <div class="relative inline-block" ref={root}>
      <button
        type="button"
        disabled={props.disabled}
        title={props.title}
        onClick={() => setOpen(v => !v)}
        class={`flex items-center gap-1 cursor-pointer disabled:cursor-not-allowed disabled:opacity-50 ${props.class ?? ""}`}
      >
        <span class="truncate">{props.options.find(o => o.value === props.value)?.label ?? props.placeholder ?? props.value}</span>
        <MaterialIcon name={open() ? "expand_less" : "expand_more"} size="sm" class="opacity-60 shrink-0" />
      </button>
      <Show when={open()}>
        <div
          class={`absolute z-50 min-w-full max-h-64 overflow-y-auto rounded-lg border border-borderColor bg-bgPanel py-1 shadow-lg ${
            props.align === "right" ? "right-0" : "left-0"
          } ${props.direction === "up" ? "bottom-full mb-1" : "top-full mt-1"} ${
            props.panelClass ?? ""
          }`}
        >
          <For each={props.options}>
            {option => (
              <button
                type="button"
                onClick={() => { props.onChange(option.value); setOpen(false); }}
                class={`flex w-full items-center px-3 py-1.5 text-left text-xs transition-colors hover:bg-bgHover ${
                  option.value === props.value ? "text-accentColor font-medium" : "text-textMain"
                }`}
              >
                <span class="truncate">{option.label}</span>
              </button>
            )}
          </For>
        </div>
      </Show>
    </div>
  );
}
