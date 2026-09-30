<script setup lang="ts">
import { onMounted, ref } from "vue";
import EmptyState from "../components/EmptyState.vue";
import ErrorBanner from "../components/ErrorBanner.vue";
import { api, formatTime } from "../lib/api";
import type { ActivityItem } from "../lib/types";

const items = ref<ActivityItem[]>([]);
const loading = ref(true);
const error = ref<string | null>(null);

async function load() {
  loading.value = true;
  error.value = null;
  try {
    items.value = await api.getActivity();
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  } finally {
    loading.value = false;
  }
}

function prepend(item: ActivityItem) {
  items.value = [item, ...items.value.filter((x) => x.tsMs !== item.tsMs || x.message !== item.message)].slice(0, 300);
}

onMounted(load);
defineExpose({ load, prepend });
</script>

<template>
  <div>
    <h2 class="mb-3 text-[15px] font-semibold">Activity</h2>
    <ErrorBanner :message="error" />
    <p v-if="loading" class="text-[var(--color-muted)]">Loading activity…</p>
    <EmptyState
      v-else-if="items.length === 0"
      title="Nothing yet"
      body="Live sync events appear here — connections, scans, and file transfers."
    />
    <ul v-else class="space-y-1">
      <li
        v-for="(item, i) in items"
        :key="`${item.tsMs}-${i}`"
        class="grid grid-cols-[72px_1fr] gap-2 rounded-md px-2 py-1.5 odd:bg-[var(--color-panel)]"
      >
        <span class="mono text-[12px] text-[var(--color-muted)]">{{ formatTime(item.tsMs) }}</span>
        <span>
          <span class="mr-2 text-[11px] uppercase tracking-wide text-[var(--color-muted)]">{{ item.kind }}</span>
          {{ item.message }}
        </span>
      </li>
    </ul>
  </div>
</template>
