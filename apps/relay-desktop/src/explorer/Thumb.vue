<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from "vue";

/**
 * A thumbnail shown by handing its decoded bitmap to a canvas, and released
 * as soon as the tile unmounts. An <img> per tile leaves every decoded
 * thumbnail in the renderer's image caches after its tile scrolls away, and
 * a 2D canvas keeps a (GPU) backing store until garbage collection; both
 * grew by hundreds of MB over a 2k-image sweep.
 */
const props = defineProps<{ url: string; size: number }>();

const canvas = ref<HTMLCanvasElement | null>(null);
const px = Math.round(props.size * window.devicePixelRatio);
let pending: AbortController | null = null;

function release() {
  const el = canvas.value;
  el?.getContext("bitmaprenderer")?.transferFromImageBitmap(null);
  if (el) el.width = el.height = 0;
}

async function show(url: string) {
  pending?.abort();
  const ctrl = new AbortController();
  pending = ctrl;
  release();
  try {
    const blob = await encoded(url, ctrl.signal);
    // Shrink to the tile while decoding; never enlarge.
    const probe = await createImageBitmap(blob);
    const scale = Math.min(1, px / probe.width, px / probe.height);
    const bitmap =
      scale < 1
        ? await createImageBitmap(probe, {
            resizeWidth: Math.max(1, Math.round(probe.width * scale)),
            resizeHeight: Math.max(1, Math.round(probe.height * scale)),
            resizeQuality: "high",
          })
        : probe;
    if (bitmap !== probe) probe.close();
    const el = canvas.value;
    if (ctrl.signal.aborted || !el) {
      bitmap.close();
      return;
    }
    el.width = bitmap.width;
    el.height = bitmap.height;
    // Takes ownership of the pixels: no copy, nothing left to close.
    el.getContext("bitmaprenderer")?.transferFromImageBitmap(bitmap);
  } catch {
    // No thumbnail (or the tile went away): leave the square empty.
  }
}

onMounted(() => watch(() => props.url, show, { immediate: true }));
onBeforeUnmount(() => {
  pending?.abort();
  release();
});
</script>

<script lang="ts">
// Recently shown thumbnails, still encoded, so scrolling back is instant
// without holding decoded pixels.
const recent = new Map<string, Blob>();
const RECENT_MAX = 256;

async function encoded(url: string, signal: AbortSignal): Promise<Blob> {
  const hit = recent.get(url);
  if (hit) {
    recent.delete(url);
    recent.set(url, hit);
    return hit;
  }
  const response = await fetch(url, { signal, cache: "no-store" });
  if (!response.ok) throw new Error(`thumbnail ${response.status}`);
  const blob = await response.blob();
  recent.set(url, blob);
  if (recent.size > RECENT_MAX) recent.delete(recent.keys().next().value as string);
  return blob;
}
</script>

<template>
  <canvas
    ref="canvas"
    width="0"
    height="0"
    class="object-contain"
    :style="{ width: `${size}px`, height: `${size}px` }"
    draggable="false"
  />
</template>
