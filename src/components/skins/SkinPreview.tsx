// The large 3D preview of the skin screen (D85): `skinview3d`, a model the
// mouse turns, and the library's idle "breathing" — arms and cape swaying a
// few hundredths of a radian.
//
// The arm model: we say `classic`/`slim` (Mojang's words, `skins.rs`),
// skinview3d says `"default"`/`"slim"` — `ModelType` in
// `skinview-utils/build/types.d.ts`, read by `PlayerObject.modelType` in
// `skinview3d/libs/model.js` as `slim = value === "slim"`. `classic` is not
// one of its values, so it is mapped; `unknown` asks skinview3d to read the
// arms off the texture.
//
// A texture that can't be loaded (an external skin or a cape, offline) is
// reported to the caller rather than left as an empty canvas.

import { createEffect, onCleanup, onMount } from "solid-js";
import { IdleAnimation, SkinViewer } from "skinview3d";
import type { SkinVariant } from "../../lib/types";

export type PreviewLoad = "loading" | "ready" | "failed";

interface SkinPreviewProps {
  skinUrl: string | null;
  variant: SkinVariant;
  capeUrl: string | null;
  onSkinLoad?: (state: PreviewLoad) => void;
  onCapeLoad?: (state: PreviewLoad) => void;
}

function modelOf(variant: SkinVariant): "default" | "slim" | "auto-detect" {
  switch (variant) {
    case "classic":
      return "default";
    case "slim":
      return "slim";
    default:
      return "auto-detect";
  }
}

export function SkinPreview(props: SkinPreviewProps) {
  let container!: HTMLDivElement;
  let canvas!: HTMLCanvasElement;
  let viewer: SkinViewer | undefined;
  let skinGeneration = 0;
  let capeGeneration = 0;

  onMount(() => {
    viewer = new SkinViewer({
      canvas,
      width: Math.max(1, container.clientWidth),
      height: Math.max(1, container.clientHeight),
      animation: new IdleAnimation(),
      zoom: 0.7,
    });
    // Turning, not zooming or sliding: the model stays framed.
    viewer.controls.enableZoom = false;
    viewer.controls.enablePan = false;
    // A quarter-turn of a quarter-turn, as Modrinth frames it, so the first
    // look already reads as 3D.
    viewer.playerWrapper.rotation.y = Math.PI / 8;

    const resize = new ResizeObserver(() => {
      viewer?.setSize(Math.max(1, container.clientWidth), Math.max(1, container.clientHeight));
    });
    resize.observe(container);
    onCleanup(() => {
      resize.disconnect();
      viewer?.dispose();
      viewer = undefined;
    });

    createEffect(() => {
      const url = props.skinUrl;
      const model = modelOf(props.variant);
      const current = viewer;
      if (!current) return;
      const mine = ++skinGeneration;
      if (!url) {
        current.loadSkin(null);
        return;
      }
      props.onSkinLoad?.("loading");
      current
        .loadSkin(url, { model })
        .then(() => mine === skinGeneration && props.onSkinLoad?.("ready"))
        .catch(() => {
          if (mine !== skinGeneration) return;
          current.loadSkin(null);
          props.onSkinLoad?.("failed");
        });
    });

    createEffect(() => {
      const url = props.capeUrl;
      const current = viewer;
      if (!current) return;
      const mine = ++capeGeneration;
      if (!url) {
        current.loadCape(null);
        props.onCapeLoad?.("ready");
        return;
      }
      props.onCapeLoad?.("loading");
      current
        .loadCape(url)
        .then(() => mine === capeGeneration && props.onCapeLoad?.("ready"))
        .catch(() => {
          if (mine !== capeGeneration) return;
          current.loadCape(null);
          props.onCapeLoad?.("failed");
        });
    });
  });

  return (
    <div ref={container} class="absolute inset-0 cursor-grab active:cursor-grabbing">
      <canvas ref={canvas} class="block" />
    </div>
  );
}
