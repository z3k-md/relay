<script setup lang="ts">
import { convertFileSrc } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from "vue";
import { api, errorText } from "./lib/api";
import type { DropArrival } from "./lib/types";

/** How long a drop stays on screen without the pointer over it. */
const SHOW_MS = 15_000;
/** How long "Copied" shows before the popup closes. */
const COPIED_MS = 1_200;

const items = ref<DropArrival[]>([]);
const index = ref(0);
const hovered = ref(false);
const copied = ref(false);
/** A copy or drag is running; the popup stays until it ends. */
const busy = ref<"copy" | "drag" | null>(null);
const error = ref<string | null>(null);
/** The popup is grown into a player for the current video. */
const playing = ref(false);
/**
 * Whether this app's WebView can show the current video. Phones often record
 * HEVC, which Windows decodes only with the codec installed; when it cannot,
 * Play opens the system player instead.
 */
const videoState = ref<"loading" | "ready" | "failed">("loading");
const current = computed(() => items.value[index.value] ?? null);
const src = computed(() => (current.value ? convertFileSrc(current.value.path) : ""));
const extension = computed(() => {
  const name = current.value?.name ?? "";
  const dot = name.lastIndexOf(".");
  return dot > 0 ? name.slice(dot + 1).toUpperCase() : "FILE";
});

watch(
  () => current.value?.path,
  () => {
    videoState.value = "loading";
  },
);

const win = getCurrentWindow();
let hideTimer: number | undefined;
/** Paths of drops the user did something with; the rest count as missed. */
const touched = new Set<string>();
/** A drag ran, so the click that may follow is not an "open". */
let dragged = false;
let press: { x: number; y: number } | null = null;
/** How far the pointer moves with the button down before a drag starts. */
const DRAG_SLOP = 5;
let unlisten: UnlistenFn | undefined;

async function take() {
  const fresh = await api.dropTake();
  if (fresh.length === 0) return;
  if (playing.value) await closePlayer();
  items.value.push(...fresh);
  // The newest drop is the one just taken on the phone.
  index.value = items.value.length - 1;
  copied.value = false;
  error.value = null;
  armHide();
}

function armHide(ms = SHOW_MS) {
  window.clearTimeout(hideTimer);
  hideTimer = window.setTimeout(() => {
    if (!hovered.value && !busy.value && !playing.value) void close(true);
  }, ms);
}

/**
 * Hide the popup. `missed`: it closed on its own, so leave a notification
 * for drops nobody used.
 */
async function close(missed = false) {
  window.clearTimeout(hideTimer);
  if (playing.value) await closePlayer();
  const unused = items.value.filter((item) => !touched.has(item.path));
  if (missed && unused.length > 0) {
    void api.dropMissed(
      unused.map((item) => item.name),
      unused[0].from,
    );
  }
  touched.clear();
  items.value = [];
  index.value = 0;
  copied.value = false;
  error.value = null;
  await win.hide();
}

async function dismiss() {
  if (playing.value) await closePlayer();
  items.value.splice(index.value, 1);
  if (items.value.length === 0) {
    await close();
    return;
  }
  index.value = Math.min(index.value, items.value.length - 1);
  copied.value = false;
  error.value = null;
  armHide();
}

async function run(action: (path: string) => Promise<void>) {
  const item = current.value;
  if (!item) return;
  touched.add(item.path);
  error.value = null;
  try {
    await action(item.path);
  } catch (err) {
    error.value = errorText(err);
  }
}

async function copy() {
  if (busy.value) return;
  busy.value = "copy";
  try {
    await run(api.dropCopy);
  } finally {
    busy.value = null;
  }
  if (error.value) return;
  copied.value = true;
  window.clearTimeout(hideTimer);
  hideTimer = window.setTimeout(() => void dismiss(), COPIED_MS);
}

function onThumbDown(event: MouseEvent) {
  if (event.button !== 0 || busy.value) return;
  event.preventDefault();
  press = { x: event.clientX, y: event.clientY };
  window.addEventListener("mousemove", onThumbMove);
  window.addEventListener("mouseup", endPress, { once: true });
}

function onThumbMove(event: MouseEvent) {
  if (!press) return;
  if (Math.hypot(event.clientX - press.x, event.clientY - press.y) < DRAG_SLOP) return;
  endPress();
  dragged = true;
  void startDrag();
}

function endPress() {
  press = null;
  window.removeEventListener("mousemove", onThumbMove);
}

/** A click on the card, not on one of its buttons or the player, opens it. */
function onCardClick(event: MouseEvent) {
  if (dragged) {
    dragged = false;
    return;
  }
  const target = event.target as HTMLElement | null;
  if (target?.closest("button, video[controls]")) return;
  void run(api.dropOpen);
}

async function startDrag() {
  if (busy.value) return;
  busy.value = "drag";
  try {
    await run(api.dropDrag);
  } finally {
    busy.value = null;
    armHide(4_000);
  }
}

async function openPlayer() {
  if (current.value) touched.add(current.value.path);
  if (videoState.value === "failed") {
    await run(api.dropOpen);
    return;
  }
  window.clearTimeout(hideTimer);
  playing.value = true;
  try {
    await api.dropPopupPlayer(true);
  } catch (err) {
    error.value = errorText(err);
  }
}

async function closePlayer() {
  playing.value = false;
  try {
    await api.dropPopupPlayer(false);
  } catch (err) {
    error.value = errorText(err);
  }
  if (!hovered.value) armHide(4_000);
}

function onPlayerError() {
  // The thumbnail frame decoded but playback did not: use the system player.
  videoState.value = "failed";
  void closePlayer().then(() => run(api.dropOpen));
}

function onEnter() {
  hovered.value = true;
  window.clearTimeout(hideTimer);
}

function onLeave() {
  hovered.value = false;
  if (!copied.value && !playing.value) armHide(4_000);
}

onMounted(async () => {
  unlisten = await listen("drop-arrived", () => void take());
  await take();
  await nextTick();
  // The window starts hidden so no blank frame flashes; show it drawn.
  await win.show();
});

onUnmounted(() => {
  window.clearTimeout(hideTimer);
  unlisten?.();
});
</script>

<template>
  <main
    class="flex h-full select-none gap-3 border border-line bg-panel p-3 text-ink"
    :class="playing ? 'flex-col' : 'cursor-pointer'"
    :title="playing ? undefined : 'Click to open'"
    @click="onCardClick"
    @mouseenter="onEnter"
    @mouseleave="onLeave"
  >
    <template v-if="current && playing">
      <video
        :src="src"
        class="aspect-video w-full rounded-md bg-black"
        controls
        autoplay
        playsinline
        @error="onPlayerError"
      />
    </template>

    <div
      v-else-if="current"
      class="relative h-[98px] w-[98px] shrink-0 cursor-grab overflow-hidden rounded-md bg-canvas active:cursor-grabbing"
      title="Click to open, drag into another app"
      @mousedown="onThumbDown"
    >
      <img
        v-if="current.image"
        :src="src"
        :alt="current.name"
        class="pointer-events-none h-full w-full object-cover"
        draggable="false"
      />
      <video
        v-else-if="current.video && videoState !== 'failed'"
        :src="`${src}#t=0.1`"
        class="pointer-events-none h-full w-full object-cover"
        preload="metadata"
        muted
        playsinline
        @loadeddata="videoState = 'ready'"
        @error="videoState = 'failed'"
      />
      <div
        v-if="!current.image && !(current.video && videoState === 'ready')"
        class="absolute inset-0 flex items-center justify-center text-sm font-semibold text-muted"
      >
        {{ extension }}
      </div>
      <button
        v-if="current.video"
        class="absolute left-1/2 top-1/2 flex h-9 w-9 -translate-x-1/2 -translate-y-1/2 items-center justify-center rounded-full bg-black/60 text-white hover:bg-black/80"
        :title="videoState === 'failed' ? 'Play in your video player' : 'Play'"
        @mousedown.stop
        @click.stop="openPlayer"
      >
        <svg viewBox="0 0 24 24" class="ml-0.5 h-4 w-4" fill="currentColor" aria-hidden="true">
          <path d="M8 5v14l11-7z" />
        </svg>
      </button>
    </div>

    <div v-if="current" class="flex min-w-0 flex-1 flex-col">
      <div class="flex items-start gap-2">
        <div class="min-w-0 flex-1">
          <p class="truncate text-xs text-muted">
            {{ current.from ? `From ${current.from}` : current.summary }}
          </p>
          <p class="truncate text-sm font-medium" :title="current.path">{{ current.name }}</p>
        </div>
        <span v-if="items.length > 1" class="shrink-0 text-xs text-muted">
          {{ index + 1 }} of {{ items.length }}
        </span>
        <button
          v-if="playing"
          class="-mt-1 shrink-0 rounded px-1.5 text-xs leading-6 text-muted hover:bg-canvas hover:text-ink"
          title="Back to the small view"
          @click="closePlayer"
        >
          Smaller
        </button>
        <button
          class="-mr-1 -mt-1 shrink-0 rounded px-1.5 text-base leading-6 text-muted hover:bg-canvas hover:text-ink"
          title="Dismiss"
          @click="dismiss"
        >
          ×
        </button>
      </div>

      <p v-if="error" class="mt-1 line-clamp-2 text-xs text-danger">{{ error }}</p>

      <div class="mt-auto flex gap-1.5" :class="playing ? 'pt-2' : ''">
        <button
          class="rounded-md bg-accent px-3 py-1 text-sm font-medium text-accent-fg hover:opacity-90"
          @click="copy"
        >
          {{ copied ? "Copied" : busy === "copy" ? "Copying…" : "Copy" }}
        </button>
        <button
          class="rounded-md border border-line px-3 py-1 text-sm hover:bg-canvas"
          @click="run(api.dropOpen)"
        >
          Open
        </button>
        <button
          class="rounded-md border border-line px-3 py-1 text-sm hover:bg-canvas"
          @click="run(api.dropReveal)"
        >
          Show in folder
        </button>
      </div>
    </div>
  </main>
</template>
