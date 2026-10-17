<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from "vue";

/**
 * A thumbnail painted from its decoded bitmap onto a canvas, loaded only
 * while near the viewport and released when it leaves. An <img> per tile
 * leaves every decoded thumbnail in the renderer's image caches, and a 2D
 * canvas keeps a GPU backing store until garbage collection; in the
 * Explorer spike both grew by hundreds of MB over a 2k-image sweep.
 */
const props = defineProps<{ url: string; size: number }>();
const emit = defineEmits<{ loaded: [ok: boolean] }>();

const canvas = ref<HTMLCanvasElement | null>(null);
let pending: AbortController | null = null;
let visible = false;

function release() {
  const el = canvas.value;
  el?.getContext("bitmaprenderer")?.transferFromImageBitmap(null);
  if (el) el.width = el.height = 0;
}

async function show(url: string) {
  pending?.abort();
  const ctrl = new AbortController();
  pending = ctrl;
  try {
    const blob = await encoded(url, ctrl.signal);
    const px = Math.round(props.size * (window.devicePixelRatio || 1));
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
    emit("loaded", true);
  } catch {
    if (!ctrl.signal.aborted) emit("loaded", false);
  }
}

function onVisibility(now: boolean) {
  if (now === visible) return;
  visible = now;
  if (now) {
    void show(props.url);
  } else {
    pending?.abort();
    pending = null;
    release();
  }
}

onMounted(() => {
  if (canvas.value) observe(canvas.value, onVisibility);
  watch(
    () => props.url,
    (url) => {
      if (visible) void show(url);
    },
  );
});

onBeforeUnmount(() => {
  if (canvas.value) unobserve(canvas.value);
  pending?.abort();
  release();
});
</script>

<script lang="ts">
// One observer for every thumbnail: a folder can hold tens of thousands.
const callbacks = new WeakMap<Element, (visible: boolean) => void>();
let observer: IntersectionObserver | null = null;

function observe(el: Element, callback: (visible: boolean) => void) {
  observer ??= new IntersectionObserver(
    (entries) => {
      for (const entry of entries) callbacks.get(entry.target)?.(entry.isIntersecting);
    },
    // Start a little before a tile scrolls in.
    { rootMargin: "300px 0px" },
  );
  callbacks.set(el, callback);
  observer.observe(el);
}

function unobserve(el: Element) {
  observer?.unobserve(el);
  callbacks.delete(el);
}

// Recently shown thumbnails, still encoded, so scrolling back is instant
// without holding decoded pixels.
const recent = new Map<string, Blob>();
const RECENT_MAX = 300;

async function encoded(url: string, signal: AbortSignal): Promise<Blob> {
  const hit = recent.get(url);
  if (hit) {
    recent.delete(url);
    recent.set(url, hit);
    return hit;
  }
  // 503: the app was busy with tiles that have since scrolled away; ask
  // again while this one is still wanted.
  for (let attempt = 0; ; attempt++) {
    const response = await fetch(url, { signal, cache: "no-store" });
    if (response.ok) {
      const blob = await response.blob();
      recent.set(url, blob);
      if (recent.size > RECENT_MAX) recent.delete(recent.keys().next().value as string);
      return blob;
    }
    if (response.status !== 503 || attempt >= 3) throw new Error(`thumbnail ${response.status}`);
    await new Promise((resolve) => setTimeout(resolve, 200 * (attempt + 1)));
    if (signal.aborted) throw new Error("aborted");
  }
}
</script>

<template>
  <canvas
    ref="canvas"
    width="0"
    height="0"
    class="block object-contain"
    :style="{ width: `${size}px`, height: `${size}px` }"
    draggable="false"
  />
</template>
