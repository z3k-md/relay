<script setup lang="ts">
import type { FilesLayout, GridSize } from "../lib/thumbs";

defineProps<{ layout: FilesLayout; gridSize: GridSize }>();
const emit = defineEmits<{ "update:layout": [FilesLayout]; "update:gridSize": [GridSize] }>();

const layouts: { value: FilesLayout; label: string; icon: string }[] = [
  { value: "list", label: "List", icon: "M3 4h10M3 8h10M3 12h10" },
  { value: "thumbs", label: "List with thumbnails", icon: "M2 3h3v3H2zM2 10h3v3H2zM7 4.5h7M7 11.5h7" },
  { value: "grid", label: "Grid", icon: "M2 2h5v5H2zM9 2h5v5H9zM2 9h5v5H2zM9 9h5v5H9z" },
];
</script>

<template>
  <div class="flex items-center gap-2">
    <select
      v-if="layout === 'grid'"
      :value="gridSize"
      aria-label="Thumbnail size"
      class="rounded-md border border-[var(--color-line)] bg-[var(--color-canvas)] px-2 py-1 text-[12px]"
      @change="emit('update:gridSize', ($event.target as HTMLSelectElement).value as GridSize)"
    >
      <option value="small">Small</option>
      <option value="medium">Medium</option>
      <option value="large">Large</option>
    </select>
    <div role="group" aria-label="View" class="flex overflow-hidden rounded-md border border-[var(--color-line)]">
      <button
        v-for="option in layouts"
        :key="option.value"
        type="button"
        :title="option.label"
        :aria-label="option.label"
        :aria-pressed="layout === option.value"
        class="px-2 py-1"
        :class="
          layout === option.value
            ? 'bg-[var(--color-panel)] text-[var(--color-ink)]'
            : 'text-[var(--color-muted)] hover:text-[var(--color-ink)]'
        "
        @click="emit('update:layout', option.value)"
      >
        <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" aria-hidden="true">
          <path :d="option.icon" stroke-linejoin="round" />
        </svg>
      </button>
    </div>
  </div>
</template>
