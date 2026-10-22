import type { JSX } from "solid-js";

export interface ToggleSwitchProps {
  checked: boolean;
  /** Called on click; the switch holds no state, the caller flips `checked`. */
  onToggle: JSX.EventHandler<HTMLButtonElement, MouseEvent>;
  disabled?: boolean;
  /** `aria-label`, when no visible label wraps the switch. */
  label?: string;
  testId?: string;
  /** Extra classes for placement (margins), not for colours. */
  class?: string;
}

/**
 * The on/off switch (`role="switch"`; named apart from Solid's `<Switch>`)
 * shared by the settings, account, shared-files and worlds panels (audit
 * 101, todo.md:1425/1439/1821): one copy, so the role, the `type="button"`
 * that keeps it from submitting a form, and the colours cannot drift apart
 * again.
 *
 * Off is a grey thumb on a dark track with a grey border, so both the thumb
 * against the track and the track against the panel stay above 3:1
 * (`muted-foreground` #a1a1aa: 5.8:1 on `muted`, 6.5:1 on `card`). The old
 * off state, a `background` thumb on a `muted` track, was about 1.3:1.
 */
export function ToggleSwitch(props: ToggleSwitchProps) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={props.checked}
      aria-label={props.label}
      disabled={props.disabled}
      data-testid={props.testId}
      onClick={props.onToggle}
      class={`h-6 w-11 shrink-0 rounded-full border transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-60 ${
        props.checked ? "border-primary bg-primary" : "border-muted-foreground bg-muted"
      } ${props.class ?? ""}`}
    >
      <span
        class={`block h-4 w-4 rounded-full transition-transform ${
          props.checked ? "translate-x-6 bg-primary-foreground" : "translate-x-1 bg-muted-foreground"
        }`}
      />
    </button>
  );
}
