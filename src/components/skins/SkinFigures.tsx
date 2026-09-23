// The flat pictures on the skin screen's cards.
//
// One WebGL viewer per card would be twenty-odd contexts for a picture that
// never moves, so a card draws the front of the player on a small 2D canvas
// instead. The texture goes through `skinview-utils` first — the same step
// the 3D preview takes — which turns a legacy 64×32 skin into 64×64 and
// clears an opaque hat layer, so a card and the preview never disagree.
//
// A saved or default card has a data URL and draws without a network. A cape
// or an external skin has Mojang's https URL: offline it fails to load, and
// the card says so instead of staying blank.

import { Show, createEffect, createSignal, on, onCleanup } from "solid-js";
import { loadCapeToCanvas, loadImage, loadSkinToCanvas } from "skinview-utils";

type LoadState = "loading" | "ready" | "failed";

/** [source x, source y, width, height, target x, target y], in 64×64 texture pixels. */
type Part = [number, number, number, number, number, number];

/** The front of the player on a 16×32 grid: base layer, then the overlay on top. */
function frontParts(slim: boolean): Part[] {
  const arm = slim ? 3 : 4;
  return [
    [8, 8, 8, 8, 4, 0], // head
    [20, 20, 8, 12, 4, 8], // body
    [44, 20, arm, 12, 4 - arm, 8], // right arm, on the viewer's left
    [36, 52, arm, 12, 12, 8], // left arm
    [4, 20, 4, 12, 4, 20], // right leg
    [20, 52, 4, 12, 8, 20], // left leg
    [40, 8, 8, 8, 4, 0], // hat
    [20, 36, 8, 12, 4, 8], // jacket
    [44, 36, arm, 12, 4 - arm, 8], // right sleeve
    [52, 52, arm, 12, 12, 8], // left sleeve
    [4, 36, 4, 12, 4, 20], // right trouser
    [4, 52, 4, 12, 8, 20], // left trouser
  ];
}

/**
 * Loads `url` and draws it with `draw` whenever the inputs change. A late
 * answer for an input that has since changed is dropped.
 */
function useTextureCanvas(
  inputs: () => readonly unknown[],
  url: () => string,
  draw: (target: HTMLCanvasElement, image: HTMLImageElement) => void,
) {
  const [state, setState] = createSignal<LoadState>("loading");
  let target: HTMLCanvasElement | undefined;
  let generation = 0;

  createEffect(
    on(inputs, () => {
      const mine = ++generation;
      setState("loading");
      loadImage(url())
        .then((image) => {
          if (mine !== generation || !target) return;
          draw(target, image);
          setState("ready");
        })
        .catch(() => {
          if (mine === generation) setState("failed");
        });
    }),
  );
  onCleanup(() => {
    generation += 1;
  });

  return { state, setTarget: (element: HTMLCanvasElement) => (target = element) };
}

function Unavailable(props: { label: string }) {
  return (
    <div class="absolute inset-0 flex items-center justify-center p-1 text-center text-[10px] leading-tight text-textMuted">
      {props.label}
    </div>
  );
}

export function SkinFigure(props: { url: string; slim: boolean; class?: string }) {
  const figure = useTextureCanvas(
    () => [props.url, props.slim],
    () => props.url,
    (target, image) => {
      const texture = document.createElement("canvas");
      loadSkinToCanvas(texture, image);
      const scale = texture.width / 64;
      target.width = 16 * scale;
      target.height = 32 * scale;
      const context = target.getContext("2d");
      if (!context) return;
      context.imageSmoothingEnabled = false;
      context.clearRect(0, 0, target.width, target.height);
      for (const [sx, sy, w, h, dx, dy] of frontParts(props.slim)) {
        context.drawImage(texture, sx * scale, sy * scale, w * scale, h * scale, dx * scale, dy * scale, w * scale, h * scale);
      }
    },
  );

  return (
    <div class={`relative ${props.class ?? ""}`}>
      <canvas
        ref={figure.setTarget}
        class="h-full w-full object-contain [image-rendering:pixelated]"
        classList={{ invisible: figure.state() !== "ready" }}
      />
      <Show when={figure.state() === "failed"}>
        <Unavailable label="Needs a connection" />
      </Show>
    </div>
  );
}

/** The outer face of a cape: 10×16 at (1, 1) of its texture. */
export function CapeFigure(props: { url: string; class?: string }) {
  const figure = useTextureCanvas(
    () => [props.url],
    () => props.url,
    (target, image) => {
      const texture = document.createElement("canvas");
      loadCapeToCanvas(texture, image);
      const scale = texture.width / 64;
      target.width = 10 * scale;
      target.height = 16 * scale;
      const context = target.getContext("2d");
      if (!context) return;
      context.imageSmoothingEnabled = false;
      context.clearRect(0, 0, target.width, target.height);
      context.drawImage(texture, 1 * scale, 1 * scale, 10 * scale, 16 * scale, 0, 0, 10 * scale, 16 * scale);
    },
  );

  return (
    <div class={`relative ${props.class ?? ""}`}>
      <canvas
        ref={figure.setTarget}
        class="h-full w-full object-contain [image-rendering:pixelated]"
        classList={{ invisible: figure.state() !== "ready" }}
      />
      <Show when={figure.state() === "failed"}>
        <Unavailable label="Needs a connection" />
      </Show>
    </div>
  );
}
