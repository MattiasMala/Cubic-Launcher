// The icon rail.
//
// Three fixed destinations (Home, Skin, Screenshots), then one icon per mod
// list, then the "new mod list" button. No names, no descriptions: with two
// mod lists a bare icon reads fine and with fifteen it does not, so every
// icon carries a tooltip and a mod list's tooltip is its name.
//
// The tooltip is ours, not the browser's `title`: `title` appears after about
// a second, in the operating system's style, outside our control — which is
// why Modrinth, whose shape this copies, does not use it either. It renders
// through a `Portal` at fixed coordinates taken from the icon's rect, so the
// scrolling mod-list column cannot clip it.

import { For, JSX, Show, createSignal } from "solid-js";
import { Portal } from "solid-js/web";
import {
  activeRailView, modListCards, selectedModListName,
  setActiveRailView, setCreateModlistModalOpen,
} from "../store";
import { MaterialIcon } from "./icons";

type HandleSelectModList = (name: string) => Promise<void>;

interface SidebarProps {
  onSelectModList: HandleSelectModList;
}

interface RailItemProps {
  tooltip: string;
  active?: boolean;
  /** A placeholder: visible, inert, and saying so in its tooltip. */
  disabled?: boolean;
  onClick?: () => void;
  children: JSX.Element;
}

function RailItem(props: RailItemProps) {
  const [anchor, setAnchor] = createSignal<HTMLButtonElement | undefined>();
  const [position, setPosition] = createSignal<{ top: number; left: number } | null>(null);

  const show = () => {
    const element = anchor();
    if (!element) return;
    const rect = element.getBoundingClientRect();
    setPosition({ top: rect.top + rect.height / 2, left: rect.right + 10 });
  };

  return (
    <>
      <button
        ref={setAnchor}
        type="button"
        // `aria-disabled` rather than the `disabled` attribute: a disabled
        // button fires no mouse events, and an icon with no name and no
        // tooltip says nothing at all.
        aria-disabled={props.disabled ? "true" : undefined}
        aria-current={props.active ? "page" : undefined}
        onClick={() => {
          if (props.disabled) return;
          props.onClick?.();
        }}
        onMouseEnter={show}
        onMouseLeave={() => setPosition(null)}
        onFocus={show}
        onBlur={() => setPosition(null)}
        class={`relative w-12 h-12 rounded-xl flex items-center justify-center shrink-0 transition-colors duration-75 ${
          props.disabled
            ? "text-textMuted/40 cursor-not-allowed"
            : props.active
              ? "bg-bgHover text-white cursor-pointer"
              : "text-textMuted hover:bg-bgHover hover:text-white cursor-pointer"
        }`}
      >
        <Show when={props.active && !props.disabled}>
          <span class="absolute -left-2 top-1/2 -translate-y-1/2 h-6 w-1 rounded-r bg-primary" />
        </Show>
        {props.children}
      </button>

      <Show when={position()}>
        {point => (
          <Portal>
            <div
              class="fixed z-[70] -translate-y-1/2 pointer-events-none"
              style={{ top: `${point().top}px`, left: `${point().left}px` }}
            >
              <div class="relative rounded-md bg-popover border border-borderColor px-2.5 py-1.5 text-xs font-medium text-textMain shadow-lg whitespace-nowrap">
                <span class="absolute -left-[5px] top-1/2 -translate-y-1/2 w-2 h-2 rotate-45 bg-popover border-l border-b border-borderColor" />
                {props.tooltip}
              </div>
            </div>
          </Portal>
        )}
      </Show>
    </>
  );
}

export function Sidebar(props: SidebarProps) {
  return (
    <aside class="w-[72px] border-r border-borderColor bg-bgPanel flex flex-col items-center shrink-0 h-full py-3 gap-1">
      <RailItem
        tooltip="Home"
        active={activeRailView() === "home"}
        onClick={() => setActiveRailView("home")}
      >
        <MaterialIcon name="home" size="lg" />
      </RailItem>

      <RailItem tooltip="Skin — not available yet" disabled>
        <MaterialIcon name="person" size="lg" />
      </RailItem>

      <RailItem
        tooltip="Screenshots"
        active={activeRailView() === "screenshots"}
        onClick={() => setActiveRailView("screenshots")}
      >
        <MaterialIcon name="photo_library" size="lg" />
      </RailItem>

      <div class="w-8 border-t border-borderColor my-2 shrink-0" />

      <div class="flex-1 min-h-0 w-full overflow-y-auto scrollbar-hide flex flex-col items-center gap-1">
        <For each={modListCards()}>
          {(modList) => {
            const isActive = () =>
              activeRailView() === "modlist" && selectedModListName() === modList.name;

            return (
              <RailItem
                tooltip={modList.displayName || modList.name}
                active={isActive()}
                onClick={() => {
                  setActiveRailView("modlist");
                  void props.onSelectModList(modList.name);
                }}
              >
                <div
                  class={`w-10 h-10 rounded-lg shadow-sm flex items-center justify-center overflow-hidden ${
                    modList.iconImage ? "" : isActive() ? "bg-primary" : "bg-muted"
                  }`}
                >
                  <Show
                    when={modList.iconImage}
                    fallback={
                      <span class="text-white font-bold text-xs">
                        {(modList.displayName || modList.name).slice(0, 3).toUpperCase() || "ML"}
                      </span>
                    }
                  >
                    <img src={modList.iconImage} class="block w-10 h-10 object-cover rounded-lg" alt="" />
                  </Show>
                </div>
              </RailItem>
            );
          }}
        </For>
      </div>

      <div class="w-8 border-t border-borderColor my-2 shrink-0" />

      <RailItem tooltip="New mod list" onClick={() => setCreateModlistModalOpen(true)}>
        <span class="w-10 h-10 rounded-lg border border-dashed border-borderColor flex items-center justify-center">
          <MaterialIcon name="add" size="md" />
        </span>
      </RailItem>
    </aside>
  );
}
