<script setup lang="ts">
import { onMounted, ref } from "vue";
import EmptyState from "../components/EmptyState.vue";
import ErrorBanner from "../components/ErrorBanner.vue";
import { api } from "../lib/api";
import type { ConflictView } from "../lib/types";

const items = ref<ConflictView[]>([]);
const loading = ref(true);
const error = ref<string | null>(null);

async function load() {
  loading.value = true;
  error.value = null;
  try {
    items.value = await api.listConflicts();
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  } finally {
    loading.value = false;
  }
}

onMounted(load);
defineExpose({ load });
</script>

<template>
  <div>
    <h2 class="mb-3 text-[15px] font-semibold">Conflicts</h2>
    <ErrorBanner :message="error" />
    <p v-if="loading" class="text-[var(--color-muted)]">Loading conflicts…</p>
    <EmptyState
      v-else-if="items.length === 0"
      title="No conflicts"
      body="When two devices edit the same file at once, Relay keeps both copies. The losing version shows up here."
    />
    <ul v-else class="space-y-2">
      <li
        v-for="(item, i) in items"
        :key="`${item.space}/${item.mount}/${item.path}-${i}`"
        class="rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)] px-3 py-2"
      >
        <p class="mono break-all text-[13px]">{{ item.space }}/{{ item.mount }}/{{ item.path }}</p>
        <p class="text-[12px] text-[var(--color-muted)]">
          From {{ item.deviceName ?? "device" }}
          <span class="mono">({{ item.deviceShort }})</span>
        </p>
      </li>
    </ul>
  </div>
</template>
