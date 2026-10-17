<script setup lang="ts">
import { reactive } from "vue";
import Thumb from "./Thumb.vue";
import { formatSize } from "../lib/format";
import { GRID_PX, LIST_THUMB_PX, hasThumb, thumbUrl } from "../lib/thumbs";
import type { FilesLayout, GridSize } from "../lib/thumbs";
import type { FileRow } from "../lib/types";

/**
 * A folder's entries as a list, a list with thumbnails, or a grid. Actions
 * are emitted; the caller decides what opening or downloading means.
 */
const props = defineProps<{
  space: string;
  mount: string;
  rows: FileRow[];
  layout: FilesLayout;
  gridSize: GridSize;
  /** Paths with an action in flight. */
  working: Set<string>;
  /** Short state text for a row ("Online only", "42%", …). */
  stateLabel: (row: FileRow) => string;
}>();

const emit = defineEmits<{
  open: [row: FileRow];
  download: [row: FileRow];
  free: [row: FileRow];
}>();

/** Paths whose thumbnail could not be made; they show the icon instead. */
const failed = reactive(new Set<string>());

function icon(row: FileRow): string {
  return row.kind === "directory" ? "📁" : row.state === "local" ? "📄" : "☁️";
}

function showThumb(row: FileRow): boolean {
  return props.layout !== "list" && hasThumb(row) && !failed.has(row.path);
}

function onThumb(row: FileRow, ok: boolean) {
  if (ok) failed.delete(row.path);
  else failed.add(row.path);
}

function openable(row: FileRow): boolean {
  return !props.working.has(row.path) && row.state !== "metadata_only" && row.state !== "stored";
}

function canDownload(row: FileRow): boolean {
  return row.kind !== "directory" && row.state === "online_only" && !props.working.has(row.path);
}

function canFree(row: FileRow): boolean {
  return row.kind !== "directory" && row.state === "local" && row.mode === "demand";
}
</script>

<template>
  <ul
    v-if="layout !== 'grid'"
    class="divide-y divide-[var(--color-line)] rounded-md border border-[var(--color-line)]"
  >
    <li
      v-for="row in rows"
      :key="row.path"
      class="flex items-center gap-2 px-3"
      :class="layout === 'thumbs' ? 'py-1' : 'py-1.5'"
    >
      <button
        type="button"
        class="flex min-w-0 flex-1 items-center gap-2 text-left"
        :class="row.kind === 'directory' ? 'font-medium' : ''"
        :disabled="!openable(row)"
        @click="emit('open', row)"
      >
        <span
          v-if="layout === 'thumbs'"
          class="flex shrink-0 items-center justify-center overflow-hidden rounded-sm text-[20px]"
          :style="{ width: `${LIST_THUMB_PX}px`, height: `${LIST_THUMB_PX}px` }"
        >
          <Thumb
            v-if="showThumb(row)"
            :url="thumbUrl(space, mount, row, LIST_THUMB_PX)"
            :size="LIST_THUMB_PX"
            @loaded="onThumb(row, $event)"
          />
          <template v-else>{{ icon(row) }}</template>
        </span>
        <span v-else class="shrink-0">{{ icon(row) }}</span>
        <span class="min-w-0 truncate">{{ row.name }}</span>
        <span v-if="row.conflict_copy" class="shrink-0 text-[11px] text-[var(--color-danger)]">conflict copy</span>
      </button>
      <span class="shrink-0 text-[12px] text-[var(--color-muted)]">{{ stateLabel(row) }}</span>
      <button
        v-if="canDownload(row)"
        type="button"
        class="shrink-0 rounded-md border border-[var(--color-line)] px-2 py-0.5 text-[12px]"
        @click="emit('download', row)"
      >
        Download
      </button>
      <button
        v-if="canFree(row)"
        type="button"
        class="shrink-0 rounded-md border border-[var(--color-line)] px-2 py-0.5 text-[12px]"
        :disabled="working.has(row.path)"
        @click="emit('free', row)"
      >
        Free up space
      </button>
      <span class="w-16 shrink-0 text-right text-[12px] text-[var(--color-muted)]">
        {{ formatSize(row.size) }}
      </span>
    </li>
  </ul>

  <ul
    v-else
    class="grid gap-2"
    :style="{ gridTemplateColumns: `repeat(auto-fill, minmax(${GRID_PX[gridSize] + 16}px, 1fr))` }"
  >
    <li
      v-for="row in rows"
      :key="row.path"
      class="group relative flex flex-col items-center rounded-md p-2 hover:bg-[var(--color-panel)]"
      :title="[row.name, formatSize(row.size), stateLabel(row)].filter(Boolean).join(' · ')"
    >
      <button
        type="button"
        class="flex w-full flex-col items-center gap-1"
        :disabled="!openable(row)"
        @click="emit('open', row)"
      >
        <span
          class="relative flex items-center justify-center overflow-hidden rounded-sm"
          :style="{
            width: `${GRID_PX[gridSize]}px`,
            height: `${GRID_PX[gridSize]}px`,
            fontSize: `${Math.round(GRID_PX[gridSize] * 0.45)}px`,
          }"
        >
          <Thumb
            v-if="showThumb(row)"
            :url="thumbUrl(space, mount, row, GRID_PX[gridSize])"
            :size="GRID_PX[gridSize]"
            @loaded="onThumb(row, $event)"
          />
          <template v-else>{{ icon(row) }}</template>
          <span
            v-if="row.kind !== 'directory' && row.state !== 'local'"
            class="absolute right-0 bottom-0 rounded-tl-sm bg-[var(--color-canvas)] px-1 text-[12px] leading-5"
            >☁️</span
          >
        </span>
        <span
          class="line-clamp-2 w-full text-center text-[12px] break-all"
          :class="row.kind === 'directory' ? 'font-medium' : ''"
        >
          {{ row.name }}
        </span>
        <span v-if="row.conflict_copy" class="text-[11px] text-[var(--color-danger)]">conflict copy</span>
        <span v-if="working.has(row.path)" class="text-[11px] text-[var(--color-muted)]">{{ stateLabel(row) }}</span>
      </button>
      <div
        v-if="canDownload(row) || canFree(row)"
        class="absolute top-1 right-1 hidden group-focus-within:block group-hover:block"
      >
        <button
          v-if="canDownload(row)"
          type="button"
          class="rounded-md border border-[var(--color-line)] bg-[var(--color-canvas)] px-1.5 py-0.5 text-[11px]"
          @click="emit('download', row)"
        >
          Download
        </button>
        <button
          v-else
          type="button"
          class="rounded-md border border-[var(--color-line)] bg-[var(--color-canvas)] px-1.5 py-0.5 text-[11px]"
          :disabled="working.has(row.path)"
          @click="emit('free', row)"
        >
          Free up space
        </button>
      </div>
    </li>
  </ul>
</template>
