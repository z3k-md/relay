<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from "vue";

/**
 * A thumbnail painted onto a canvas from a bitmap that is closed right after.
 * An <img> per tile leaves each decoded thumbnail in the renderer's image
 * caches after its tile scrolls away (about 200 MB after sweeping 2k images),
 * whereas a canvas keeps decoded pixels to the tiles that exist.
 */
const props = defineProps<{ url: string; size: number }>();

const canvas = ref<HTMLCanvasElement | null>(null);
const px = Math.round(props.size * window.devicePixelRatio);
let pending: AbortController | null = null;

async function draw(url: string) {
  pending?.abort();
  const ctrl = new AbortController();
  pending = ctrl;
  const ctx = canvas.value?.getContext("2d");
  ctx?.clearRect(0, 0, px, px);
  try {
    const bitmap = await createImageBitmap(await encoded(url, ctrl.signal));
    if (ctrl.signal.aborted || !ctx) {
      bitmap.close();
      return;
    }
    // Fit inside the square and centre it, like object-fit: contain.
    const scale = Math.min(px / bitmap.width, px / bitmap.height);
    const w = Math.round(bitmap.width * scale);
    const h = Math.round(bitmap.height * scale);
    ctx.drawImage(bitmap, (px - w) >> 1, (px - h) >> 1, w, h);
    bitmap.close();
  } catch {
    // No thumbnail (or the tile went away): leave the square empty.
  }
}

onMounted(() => watch(() => props.url, draw, { immediate: true }));
onBeforeUnmount(() => pending?.abort());
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
    :width="px"
    :height="px"
    :style="{ width: `${size}px`, height: `${size}px` }"
    draggable="false"
  />
</template>
