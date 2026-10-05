<script setup lang="ts">
import { Channel } from "@tauri-apps/api/core";
import { computed, nextTick, onBeforeUnmount, onMounted, ref, shallowRef, watch } from "vue";
import ItemsView from "./ItemsView.vue";
import PerfPanel from "./PerfPanel.vue";
import { autobench } from "./autobench";
import { nextFrame } from "./bench";
import {
  type DragEvent,
  EFFECT_COPY,
  EFFECT_MOVE,
  type Entry,
  FLAG_DIR,
  type Places,
  baseName,
  explorer,
  joinPath,
  parentPath,
} from "./api";
import { type Folder, createFolder } from "./folder";

const places = ref<Places | null>(null);
const tabs = shallowRef<Folder[]>([]);
const active = ref(0);
const tab = computed<Folder | undefined>(() => tabs.value[active.value]);
const address = ref("");
const addressInput = ref<HTMLInputElement | null>(null);
const items = ref<InstanceType<typeof ItemsView> | null>(null);
const showPerf = ref(true);
const native = computed(() => places.value?.native ?? false);

const dnd = ref("Native drag and drop: not set up");
const dropTarget = ref<string | null>(null);
const lastDrag = ref("");
const lastDrop = ref("");
const notices = ref<{ id: number; text: string; error: boolean }[]>([]);
/** Progress of the one-shot benchmark (RELAY_EXPLORER_BENCH). */
const benchStatus = ref<string | null>(null);
let noticeId = 0;

function notify(text: string, error = false) {
  const id = ++noticeId;
  notices.value = [...notices.value, { id, text, error }].slice(-4);
  setTimeout(() => (notices.value = notices.value.filter((n) => n.id !== id)), error ? 8000 : 4000);
}

watch(
  () => tab.value?.path.value,
  (path) => {
    address.value = path ?? "";
    document.title = path ? `${baseName(path)} - Relay Explorer (preview)` : "Relay Explorer (preview)";
  },
);

function openTab(path: string): Folder {
  const folder = createFolder(path);
  tabs.value = [...tabs.value, folder];
  active.value = tabs.value.length - 1;
  void folder.load(path);
  return folder;
}

function closeTab(index: number) {
  if (tabs.value.length === 1) return;
  tabs.value[index].close();
  tabs.value = tabs.value.filter((_, i) => i !== index);
  if (active.value >= tabs.value.length) active.value = tabs.value.length - 1;
  else if (active.value > index) active.value--;
}

function go(path: string) {
  tab.value?.navigate(path);
}

function up() {
  const parent = tab.value && parentPath(tab.value.path.value);
  if (parent) go(parent);
}

function submitAddress() {
  const path = address.value.trim();
  if (path) go(path);
  items.value?.scroller?.focus();
}

async function open(entry: Entry) {
  const folder = tab.value;
  if (!folder) return;
  const path = joinPath(folder.path.value, entry[0]);
  if (entry[1] & FLAG_DIR) {
    folder.navigate(path);
    return;
  }
  try {
    await explorer.openItem(path);
  } catch (err) {
    notify(`Couldn't open ${entry[0]}: ${err}`, true);
  }
}

async function menu(event: MouseEvent, names: string[]) {
  const folder = tab.value;
  if (!folder || !native.value) return;
  try {
    const verb = await explorer.contextMenu(
      folder.path.value,
      names,
      event.clientX,
      event.clientY,
      event.shiftKey,
    );
    if (verb === "rename") notify("Rename in place comes with P1.");
  } catch (err) {
    notify(`Context menu failed: ${err}`, true);
  }
}

async function dragOut(names: string[]) {
  const folder = tab.value;
  if (!folder || names.length === 0) return;
  try {
    const effect = await explorer.startDrag(folder.path.value, names);
    lastDrop.value = `Drag out of ${names.length} item(s): target reported ${effectName(effect)}`;
  } catch (err) {
    notify(`Drag failed: ${err}`, true);
  }
}

function effectName(effect: number): string {
  if (effect & EFFECT_MOVE) return "move";
  if (effect & EFFECT_COPY) return "copy";
  return effect ? `effect ${effect}` : "none";
}

// --- native drop target --------------------------------------------------

let reportedTarget: string | null | undefined;

function setDropTarget(path: string | null) {
  dropTarget.value = path;
  if (path !== reportedTarget) {
    reportedTarget = path;
    void explorer.dropTarget(path);
  }
}

/** Which folder is under a drag at physical client pixel `x`, `y`. */
function folderAt(x: number, y: number): string | null {
  const ratio = window.devicePixelRatio;
  const el = document.elementFromPoint(x / ratio, y / ratio) as HTMLElement | null;
  const row = el?.closest<HTMLElement>("[data-drop-dir]");
  if (row?.dataset.dropDir) return row.dataset.dropDir;
  if (el?.closest("[data-drop-zone]")) return tab.value?.path.value ?? null;
  return null;
}

function onDrag(event: DragEvent) {
  switch (event.kind) {
    case "over":
      setDropTarget(folderAt(event.x, event.y));
      if (event.enter) {
        lastDrag.value = `${event.count} path(s), ${event.virtualCount} virtual file(s); formats: ${event.formats.join(", ")}`;
      }
      break;
    case "leave":
      setDropTarget(null);
      break;
    case "dropped":
      setDropTarget(null);
      if (event.effect) {
        lastDrop.value = `${effectName(event.effect)} ${event.count} item(s) to ${event.target}…`;
      } else {
        lastDrop.value = `Dropped ${event.count} item(s) on ${event.target}: nothing to do`;
      }
      break;
    case "done":
      lastDrop.value = event.ok
        ? `Finished into ${event.target} in ${event.elapsedMs.toFixed(0)} ms`
        : `Into ${event.target}: ${event.message}`;
      notify(lastDrop.value, !event.ok && event.message !== "cancelled");
      break;
  }
}

async function setUpDragAndDrop() {
  const channel = new Channel<DragEvent>();
  channel.onmessage = onDrag;
  try {
    const on = await explorer.ready(channel);
    dnd.value = on ? "Native drag and drop: on" : "Native drag and drop: not available here";
  } catch (err) {
    dnd.value = `Native drag and drop failed: ${err}`;
  }
}

// --- keyboard -------------------------------------------------------------

function onKey(event: KeyboardEvent) {
  const inInput = event.target instanceof HTMLInputElement;
  if (event.altKey && event.key === "ArrowLeft") tab.value?.goBack();
  else if (event.altKey && event.key === "ArrowRight") tab.value?.goForward();
  else if (event.altKey && event.key === "ArrowUp") up();
  else if (event.key === "Backspace" && !inInput) up();
  else if (event.key === "F5") tab.value?.load(tab.value.path.value, false);
  else if (event.ctrlKey && event.key === "l") {
    addressInput.value?.focus();
    addressInput.value?.select();
  } else if (event.ctrlKey && event.key === "t") openTab(tab.value?.path.value ?? places.value?.home ?? "/");
  else if (event.ctrlKey && event.key === "w") closeTab(active.value);
  else if (event.ctrlKey && event.key === "Tab") {
    active.value = (active.value + (event.shiftKey ? tabs.value.length - 1 : 1)) % tabs.value.length;
  } else return;
  event.preventDefault();
}

onMounted(async () => {
  window.addEventListener("keydown", onKey);
  places.value = await explorer.places();
  openTab(places.value.home);
  await nextTick();
  items.value?.scroller?.focus();
  if (places.value.native) await setUpDragAndDrop();
  const output = await explorer.autobench().catch(() => null);
  if (output) await runAutobench(output, places.value.home);
});

async function runAutobench(output: string, home: string) {
  const status = (text: string) => (benchStatus.value = `Benchmark → ${output}: ${text}`);
  status("starting…");
  const report = await autobench({
    home,
    active: () => tabs.value[active.value],
    openTab: async (path) => {
      const folder = openTab(path);
      await nextTick();
      await nextFrame();
      return folder;
    },
    scroller: () => items.value?.scroller ?? null,
    tabs: () => tabs.value.length,
    status,
  });
  status("saving results and quitting…");
  await explorer.saveResults(report);
}

onBeforeUnmount(() => window.removeEventListener("keydown", onKey));

const status = computed(() => {
  const folder = tab.value;
  if (!folder) return "";
  const count = folder.rows.value.length;
  const selected = folder.selected.value.size;
  const parts = [`${count.toLocaleString()} item${count === 1 ? "" : "s"}`];
  if (selected) parts.push(`${selected.toLocaleString()} selected`);
  if (folder.loading.value) parts.push("loading…");
  return parts.join("   ");
});
</script>

<template>
  <div class="flex h-full flex-col bg-canvas text-[13px] text-ink select-none">
    <nav class="flex shrink-0 items-end gap-1 overflow-x-auto px-2 pt-1.5">
      <div
        v-for="(t, i) in tabs"
        :key="t.id"
        class="group flex max-w-52 min-w-28 cursor-default items-center gap-1 rounded-t-md border border-b-0 px-3 py-1.5"
        :class="i === active ? 'border-line bg-panel' : 'border-transparent text-muted hover:bg-panel/60'"
        @click="active = i"
        @auxclick.middle="closeTab(i)"
      >
        <span class="flex-1 truncate">{{ baseName(t.path.value) }}</span>
        <button
          v-if="tabs.length > 1"
          class="rounded px-1 text-muted opacity-0 group-hover:opacity-100 hover:bg-canvas"
          title="Close tab (Ctrl+W)"
          @click.stop="closeTab(i)"
        >
          ×
        </button>
      </div>
      <button
        class="mb-1 rounded px-2 py-0.5 text-muted hover:bg-panel"
        title="New tab (Ctrl+T)"
        @click="openTab(tab?.path.value ?? places?.home ?? '/')"
      >
        +
      </button>
    </nav>

    <p v-if="benchStatus" class="shrink-0 bg-accent px-3 py-1 text-xs text-accent-fg">
      {{ benchStatus }}
    </p>
    <header v-if="tab" class="flex shrink-0 items-center gap-1 border-y border-line bg-panel px-2 py-1.5">
      <button
        class="rounded px-2 py-1 hover:bg-canvas disabled:opacity-40"
        :disabled="!tab.canBack()"
        title="Back (Alt+Left)"
        @click="tab.goBack()"
      >
        ←
      </button>
      <button
        class="rounded px-2 py-1 hover:bg-canvas disabled:opacity-40"
        :disabled="!tab.canForward()"
        title="Forward (Alt+Right)"
        @click="tab.goForward()"
      >
        →
      </button>
      <button class="rounded px-2 py-1 hover:bg-canvas" title="Up (Alt+Up)" @click="up">↑</button>
      <button
        class="rounded px-2 py-1 hover:bg-canvas"
        title="Refresh (F5)"
        @click="tab.load(tab.path.value, false)"
      >
        ⟳
      </button>
      <form class="flex flex-1" @submit.prevent="submitAddress">
        <input
          ref="addressInput"
          v-model="address"
          spellcheck="false"
          class="w-full rounded border border-line bg-canvas px-2 py-1 outline-none select-text focus:border-accent"
        />
      </form>
      <div class="ml-2 flex overflow-hidden rounded border border-line">
        <button
          class="px-2 py-1"
          :class="tab.view.value === 'details' ? 'bg-accent text-accent-fg' : 'hover:bg-canvas'"
          @click="tab.view.value = 'details'"
        >
          Details
        </button>
        <button
          class="px-2 py-1"
          :class="tab.view.value === 'grid' ? 'bg-accent text-accent-fg' : 'hover:bg-canvas'"
          @click="tab.view.value = 'grid'"
        >
          Grid
        </button>
      </div>
      <label class="ml-2 flex items-center gap-1 text-muted">
        <input
          type="checkbox"
          :checked="tab.showHidden.value"
          @change="tab.setShowHidden(($event.target as HTMLInputElement).checked)"
        />
        Hidden
      </label>
      <button
        class="ml-2 rounded px-2 py-1 hover:bg-canvas"
        :class="showPerf ? 'text-accent' : 'text-muted'"
        @click="showPerf = !showPerf"
      >
        Perf
      </button>
    </header>

    <div class="flex min-h-0 flex-1">
      <aside class="w-44 shrink-0 overflow-auto border-r border-line bg-panel py-2">
        <template v-if="places">
          <button
            class="block w-full truncate px-3 py-1 text-left hover:bg-canvas"
            @click="go(places.home)"
          >
            Home
          </button>
          <button
            v-for="root in places.roots"
            :key="root"
            class="block w-full truncate px-3 py-1 text-left hover:bg-canvas"
            @click="go(root)"
          >
            {{ root }}
          </button>
        </template>
      </aside>

      <main v-if="tab" class="flex min-w-0 flex-1 flex-col">
        <p v-if="tab.error.value" class="border-b border-line bg-panel px-3 py-2 text-danger">
          {{ tab.error.value }}
        </p>
        <ItemsView
          ref="items"
          :key="tab.id"
          :folder="tab"
          :native="native"
          :drop-target="dropTarget"
          @open="open"
          @menu="menu"
          @drag-out="dragOut"
        />
        <footer class="flex h-6 shrink-0 items-center border-t border-line bg-panel px-3 text-xs text-muted">
          {{ status }}
        </footer>
      </main>

      <PerfPanel
        v-if="showPerf && tab"
        :folder="tab"
        :tabs="tabs.length"
        :native="native"
        :dnd="dnd"
        :last-drag="lastDrag"
        :last-drop="lastDrop"
        :get-scroller="() => items?.scroller ?? null"
        @navigate="go"
      />
    </div>

    <div class="pointer-events-none fixed right-4 bottom-8 flex flex-col items-end gap-2">
      <div
        v-for="n in notices"
        :key="n.id"
        class="max-w-md rounded border bg-panel px-3 py-2 shadow"
        :class="n.error ? 'border-danger text-danger' : 'border-line'"
      >
        {{ n.text }}
      </div>
    </div>
  </div>
</template>
