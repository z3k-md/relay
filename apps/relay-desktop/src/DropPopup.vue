<script setup lang="ts">
import { convertFileSrc } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { computed, nextTick, onMounted, onUnmounted, ref } from "vue";
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
const error = ref<string | null>(null);
const current = computed(() => items.value[index.value] ?? null);
const extension = computed(() => {
  const name = current.value?.name ?? "";
  const dot = name.lastIndexOf(".");
  return dot > 0 ? name.slice(dot + 1).toUpperCase() : "FILE";
});

const win = getCurrentWindow();
let hideTimer: number | undefined;
let unlisten: UnlistenFn | undefined;

async function take() {
  const fresh = await api.dropTake();
  if (fresh.length === 0) return;
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
    if (!hovered.value) void close();
  }, ms);
}

async function close() {
  window.clearTimeout(hideTimer);
  items.value = [];
  index.value = 0;
  copied.value = false;
  error.value = null;
  await win.hide();
}

async function dismiss() {
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
  error.value = null;
  try {
    await action(item.path);
  } catch (err) {
    error.value = errorText(err);
  }
}

async function copy() {
  await run(api.dropCopy);
  if (error.value) return;
  copied.value = true;
  window.clearTimeout(hideTimer);
  hideTimer = window.setTimeout(() => void dismiss(), COPIED_MS);
}

function startDrag(event: MouseEvent) {
  if (event.button !== 0) return;
  event.preventDefault();
  void run(api.dropDrag);
}

function onEnter() {
  hovered.value = true;
  window.clearTimeout(hideTimer);
}

function onLeave() {
  hovered.value = false;
  if (!copied.value) armHide(4_000);
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
    @mouseenter="onEnter"
    @mouseleave="onLeave"
  >
    <template v-if="current">
      <div
        class="relative h-[98px] w-[98px] shrink-0 cursor-grab overflow-hidden rounded-md bg-canvas active:cursor-grabbing"
        title="Drag into another app"
        @mousedown="startDrag"
      >
        <img
          v-if="current.image"
          :src="convertFileSrc(current.path)"
          :alt="current.name"
          class="pointer-events-none h-full w-full object-cover"
          draggable="false"
        />
        <div
          v-else
          class="flex h-full w-full items-center justify-center text-sm font-semibold text-muted"
        >
          {{ extension }}
        </div>
      </div>

      <div class="flex min-w-0 flex-1 flex-col">
        <div class="flex items-start gap-2">
          <div class="min-w-0 flex-1">
            <p class="truncate text-xs text-muted">{{ current.summary }}</p>
            <p class="truncate text-sm font-medium" :title="current.path">{{ current.name }}</p>
          </div>
          <span v-if="items.length > 1" class="shrink-0 text-xs text-muted">
            {{ index + 1 }} of {{ items.length }}
          </span>
          <button
            class="-mr-1 -mt-1 shrink-0 rounded px-1.5 text-base leading-6 text-muted hover:bg-canvas hover:text-ink"
            title="Dismiss"
            @click="dismiss"
          >
            ×
          </button>
        </div>

        <p v-if="error" class="mt-1 line-clamp-2 text-xs text-danger">{{ error }}</p>

        <div class="mt-auto flex gap-1.5">
          <button
            class="rounded-md bg-accent px-3 py-1 text-sm font-medium text-accent-fg hover:opacity-90"
            @click="copy"
          >
            {{ copied ? "Copied" : "Copy" }}
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
    </template>
  </main>
</template>
