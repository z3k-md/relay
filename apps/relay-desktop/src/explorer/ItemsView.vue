<script setup lang="ts">
import { useVirtualizer } from "@tanstack/vue-virtual";
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from "vue";
import {
  type Entry,
  FLAG_CLOUD,
  FLAG_DIR,
  FLAG_HIDDEN,
  FLAG_LINK,
  iconUrl,
  joinPath,
  thumbUrl,
} from "./api";
import type { Folder, SortKey } from "./folder";

const props = defineProps<{
  folder: Folder;
  native: boolean;
  /** Folder highlighted as the drop target during a native drag. */
  dropTarget: string | null;
}>();

const emit = defineEmits<{
  open: [entry: Entry];
  menu: [event: MouseEvent, names: string[]];
  dragOut: [names: string[]];
}>();

const ROW = 28;
const TILE_W = 124;
const TILE_H = 140;
const THUMB = 96;

const scroller = ref<HTMLElement | null>(null);
const width = ref(800);

const rows = computed(() => props.folder.rows.value);
const selected = computed(() => props.folder.selected.value);
const dir = computed(() => props.folder.path.value);
const grid = computed(() => props.folder.view.value === "grid");
const columns = computed(() => (grid.value ? Math.max(1, Math.floor(width.value / TILE_W)) : 1));
const lines = computed(() => Math.ceil(rows.value.length / columns.value));

const virtualizer = useVirtualizer(
  computed(() => ({
    count: lines.value,
    getScrollElement: () => scroller.value,
    estimateSize: () => (grid.value ? TILE_H : ROW),
    overscan: grid.value ? 2 : 10,
    initialOffset: props.folder.scrollTop,
  })),
);
const items = computed(() => virtualizer.value.getVirtualItems());
const total = computed(() => virtualizer.value.getTotalSize());

watch(grid, async () => {
  await nextTick();
  virtualizer.value.measure();
});

let resize: ResizeObserver | null = null;
onMounted(() => {
  if (scroller.value) {
    width.value = scroller.value.clientWidth;
    resize = new ResizeObserver(([entry]) => (width.value = entry.contentRect.width));
    resize.observe(scroller.value);
    scroller.value.scrollTop = props.folder.scrollTop;
  }
});
onBeforeUnmount(() => resize?.disconnect());

function onScroll() {
  if (scroller.value) props.folder.scrollTop = scroller.value.scrollTop;
}

function line(index: number): { entry: Entry; index: number }[] {
  const start = index * columns.value;
  const out = [];
  for (let i = start; i < Math.min(start + columns.value, rows.value.length); i++) {
    out.push({ entry: rows.value[i], index: i });
  }
  return out;
}

// --- formatting -------------------------------------------------------

const dates = new Intl.DateTimeFormat(undefined, { dateStyle: "short", timeStyle: "short" });

function modified(e: Entry): string {
  return e[3] > 0 ? dates.format(e[3]) : "";
}

function kind(e: Entry): string {
  if (e[1] & FLAG_DIR) return "File folder";
  const dot = e[0].lastIndexOf(".");
  return dot > 0 ? `${e[0].slice(dot + 1).toUpperCase()} File` : "File";
}

function size(e: Entry): string {
  if (e[1] & FLAG_DIR) return "";
  // Explorer rounds up to whole KB.
  return `${Math.ceil(e[2] / 1024).toLocaleString()} KB`;
}

/** No icon is better than a broken-image glyph. */
function hideBroken(event: Event) {
  (event.target as HTMLImageElement).style.visibility = "hidden";
}

const isDir = (e: Entry) => (e[1] & FLAG_DIR) !== 0;
const fullPath = (e: Entry) => joinPath(dir.value, e[0]);

// --- selection, opening, menus, drag out --------------------------------

function setSelection(names: Iterable<string>, anchor: number | null) {
  props.folder.selected.value = new Set(names);
  props.folder.anchor.value = anchor;
}

function selectRange(to: number, add: boolean) {
  const from = props.folder.anchor.value ?? to;
  const [lo, hi] = from < to ? [from, to] : [to, from];
  const names = add ? new Set(selected.value) : new Set<string>();
  for (let i = lo; i <= hi; i++) names.add(rows.value[i][0]);
  props.folder.selected.value = names;
}

let press: { x: number; y: number; index: number; collapse: boolean } | null = null;

function onItemDown(event: PointerEvent, index: number) {
  scroller.value?.focus({ preventScroll: true });
  if (event.button !== 0) return;
  const name = rows.value[index][0];
  let collapse = false;
  if (event.shiftKey) {
    selectRange(index, event.ctrlKey);
  } else if (event.ctrlKey) {
    const names = new Set(selected.value);
    if (names.has(name)) names.delete(name);
    else names.add(name);
    setSelection(names, index);
  } else if (selected.value.has(name)) {
    // Keep a multi-selection until we know this is not a drag.
    collapse = selected.value.size > 1;
    props.folder.anchor.value = index;
  } else {
    setSelection([name], index);
  }
  press = { x: event.clientX, y: event.clientY, index, collapse };
}

function onPointerMove(event: PointerEvent) {
  if (!press || !props.native || !(event.buttons & 1)) return;
  if (Math.hypot(event.clientX - press.x, event.clientY - press.y) < 6) return;
  press = null;
  emit("dragOut", [...selected.value]);
}

function onPointerUp() {
  if (press?.collapse) setSelection([rows.value[press.index][0]], press.index);
  press = null;
}

function onBackgroundDown(event: PointerEvent) {
  if (event.target === event.currentTarget || (event.target as HTMLElement).dataset.spacer) {
    scroller.value?.focus({ preventScroll: true });
    if (!event.ctrlKey && !event.shiftKey) setSelection([], null);
  }
}

function onItemMenu(event: MouseEvent, index: number) {
  const name = rows.value[index][0];
  if (!selected.value.has(name)) setSelection([name], index);
  emit("menu", event, [...props.folder.selected.value]);
}

function onBackgroundMenu(event: MouseEvent) {
  setSelection([], null);
  emit("menu", event, []);
}

function move(delta: number, event: KeyboardEvent) {
  if (rows.value.length === 0) return;
  const current = props.folder.anchor.value ?? -1;
  const next = Math.max(0, Math.min(rows.value.length - 1, current + delta));
  if (event.shiftKey) {
    if (props.folder.anchor.value === null) props.folder.anchor.value = next;
    const anchor = props.folder.anchor.value;
    selectRange(next, false);
    props.folder.anchor.value = anchor;
  } else {
    setSelection([rows.value[next][0]], next);
  }
  virtualizer.value.scrollToIndex(Math.floor(next / columns.value), { align: "auto" });
}

function onKey(event: KeyboardEvent) {
  const cols = columns.value;
  const page = Math.max(1, Math.floor((scroller.value?.clientHeight ?? 400) / (grid.value ? TILE_H : ROW)));
  switch (event.key) {
    case "ArrowDown":
      move(cols, event);
      break;
    case "ArrowUp":
      move(-cols, event);
      break;
    case "ArrowRight":
      if (!grid.value) return;
      move(1, event);
      break;
    case "ArrowLeft":
      if (!grid.value) return;
      move(-1, event);
      break;
    case "PageDown":
      move(page * cols, event);
      break;
    case "PageUp":
      move(-page * cols, event);
      break;
    case "Home":
      move(-rows.value.length, event);
      break;
    case "End":
      move(rows.value.length, event);
      break;
    case "Enter": {
      const index = props.folder.anchor.value;
      if (index !== null && rows.value[index]) emit("open", rows.value[index]);
      break;
    }
    case "a":
      if (!event.ctrlKey) return;
      setSelection(
        rows.value.map((e) => e[0]),
        props.folder.anchor.value,
      );
      break;
    default:
      return;
  }
  event.preventDefault();
}

function sortMark(key: SortKey): string {
  if (props.folder.sortKey.value !== key) return "";
  return props.folder.sortDesc.value ? "▾" : "▴";
}

defineExpose({ scroller });
</script>

<template>
  <div class="flex min-h-0 flex-1 flex-col">
    <div
      v-if="!grid"
      class="flex h-7 shrink-0 items-center border-b border-line bg-panel pr-3 text-xs text-muted"
    >
      <button class="flex-1 truncate px-3 text-left hover:text-ink" @click="folder.sortBy('name')">
        Name {{ sortMark("name") }}
      </button>
      <button class="w-40 px-2 text-left hover:text-ink" @click="folder.sortBy('modified')">
        Date modified {{ sortMark("modified") }}
      </button>
      <button class="w-28 px-2 text-left hover:text-ink" @click="folder.sortBy('type')">
        Type {{ sortMark("type") }}
      </button>
      <button class="w-24 px-2 text-right hover:text-ink" @click="folder.sortBy('size')">
        Size {{ sortMark("size") }}
      </button>
    </div>
    <div
      ref="scroller"
      tabindex="0"
      data-drop-zone
      class="relative min-h-0 flex-1 overflow-auto bg-panel"
      style="outline: none"
      @scroll.passive="onScroll"
      @keydown="onKey"
      @pointerdown="onBackgroundDown"
      @pointermove="onPointerMove"
      @pointerup="onPointerUp"
      @contextmenu.prevent.self="onBackgroundMenu"
      @dragstart.prevent
    >
      <div
        data-spacer="1"
        class="relative w-full"
        :style="{ height: `${total}px` }"
        @contextmenu.prevent.self="onBackgroundMenu"
      >
        <template v-if="!grid">
          <div
            v-for="item in items"
            :key="item.key as number"
            class="absolute top-0 left-0 flex w-full items-center pr-3"
            :class="[
              selected.has(rows[item.index][0])
                ? 'bg-accent/15'
                : 'hover:bg-canvas',
              dropTarget !== null && isDir(rows[item.index]) && dropTarget === fullPath(rows[item.index])
                ? 'outline-2 -outline-offset-2 outline-accent'
                : '',
              rows[item.index][1] & FLAG_HIDDEN ? 'opacity-60' : '',
            ]"
            :style="{ height: `${ROW}px`, transform: `translateY(${item.start}px)` }"
            :data-drop-dir="isDir(rows[item.index]) ? fullPath(rows[item.index]) : undefined"
            @pointerdown="onItemDown($event, item.index)"
            @dblclick="emit('open', rows[item.index])"
            @contextmenu.prevent.stop="onItemMenu($event, item.index)"
          >
            <span class="flex min-w-0 flex-1 items-center gap-2 px-3">
              <img
                v-if="native"
                :src="iconUrl(dir, rows[item.index], 16)"
                width="16"
                height="16"
                alt=""
                draggable="false"
                decoding="async"
                class="shrink-0"
                @error="hideBroken"
              />
              <span v-else class="w-4 shrink-0 text-center text-xs text-muted">
                {{ isDir(rows[item.index]) ? "▸" : "·" }}
              </span>
              <span class="truncate">{{ rows[item.index][0] }}</span>
              <span v-if="rows[item.index][1] & FLAG_LINK" class="text-xs text-muted" title="Link">↗</span>
              <span v-if="rows[item.index][1] & FLAG_CLOUD" class="text-xs text-muted" title="Online only">☁</span>
            </span>
            <span class="w-40 truncate px-2 text-muted">{{ modified(rows[item.index]) }}</span>
            <span class="w-28 truncate px-2 text-muted">{{ kind(rows[item.index]) }}</span>
            <span class="w-24 px-2 text-right text-muted tabular-nums">{{ size(rows[item.index]) }}</span>
          </div>
        </template>
        <template v-else>
          <div
            v-for="item in items"
            :key="item.key as number"
            class="absolute top-0 left-0 flex w-full px-1"
            :style="{ height: `${TILE_H}px`, transform: `translateY(${item.start}px)` }"
            data-spacer="1"
            @contextmenu.prevent.self="onBackgroundMenu"
          >
            <div
              v-for="cell in line(item.index)"
              :key="cell.entry[0]"
              class="m-0.5 flex flex-col items-center gap-1 rounded p-1"
              :class="[
                selected.has(cell.entry[0]) ? 'bg-accent/15' : 'hover:bg-canvas',
                dropTarget !== null && isDir(cell.entry) && dropTarget === fullPath(cell.entry)
                  ? 'outline-2 -outline-offset-2 outline-accent'
                  : '',
                cell.entry[1] & FLAG_HIDDEN ? 'opacity-60' : '',
              ]"
              :style="{ width: `${TILE_W - 4}px` }"
              :data-drop-dir="isDir(cell.entry) ? fullPath(cell.entry) : undefined"
              @pointerdown="onItemDown($event, cell.index)"
              @dblclick="emit('open', cell.entry)"
              @contextmenu.prevent.stop="onItemMenu($event, cell.index)"
            >
              <div class="flex h-24 w-24 items-center justify-center">
                <img
                  v-if="native"
                  :src="thumbUrl(dir, cell.entry, THUMB)"
                  :width="THUMB"
                  :height="THUMB"
                  alt=""
                  draggable="false"
                  decoding="async"
                  class="max-h-24 max-w-24 object-contain"
                  @error="hideBroken"
                />
                <span v-else class="text-3xl text-muted">{{ isDir(cell.entry) ? "▸" : "·" }}</span>
              </div>
              <span class="line-clamp-2 w-full text-center text-xs break-all">{{ cell.entry[0] }}</span>
            </div>
          </div>
        </template>
      </div>
    </div>
  </div>
</template>
