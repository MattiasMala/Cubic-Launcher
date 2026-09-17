import { Show } from "solid-js";
import type { ModRow } from "../lib/types";
import { modIcons } from "../store";
import { PackageIcon } from "./icons";

/**
 * Tiny inline mod icon.
 *
 * Takes the row rather than a project id so that "where a mod's icon comes
 * from" lives here and not in nineteen call sites: a Modrinth row uses the CDN
 * icon from `modIcons`, a local row the one extracted from its jar
 * (`mod_icons.rs`), and a row with neither keeps the package glyph. Same
 * precedence as the mod list row (`ModRuleItem.tsx`).
 *
 * `name` is a prop of its own because callers often show a different label
 * than `row.name` (a link partner's resolved name, for instance).
 */
export function ModIcon(props: { row?: ModRow; name?: string; class?: string }) {
  const size = () => props.class ?? "h-4 w-4";
  const url = () =>
    (props.row?.modrinth_id ? modIcons().get(props.row.modrinth_id) : undefined) ?? props.row?.iconImage;
  return (
    <div class={`${size()} shrink-0 overflow-hidden rounded`}>
      <Show when={url()} fallback={<PackageIcon class={`${size()} text-muted-foreground`} />}>
        <img src={url()!} alt={props.name ?? props.row?.name ?? ""} class={`${size()} object-cover`} onError={e => { e.currentTarget.style.display = "none"; }} />
      </Show>
    </div>
  );
}
