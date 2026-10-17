<script setup lang="ts">
import { onMounted, ref } from "vue";
import { api } from "../lib/api";
import { describeRemoteError } from "../lib/remoteFolders";
import type { DirEntry, RemoteRoot } from "../lib/types";

/** Pick a folder on another device by browsing it (D37). */
const props = defineProps<{
  peer: string;
  start?: string | null;
}>();

const emit = defineEmits<{
  pick: [path: string];
}>();

const roots = ref<RemoteRoot[]>([]);
const path = ref<string | null>(null);
const parent = ref<string | null>(null);
const folders = ref<DirEntry[]>([]);
const loading = ref(false);
const error = ref<string | null>(null);

async function list(target: string) {
  loading.value = true;
  error.value = null;
  try {
    const reply = await api.remoteCall(props.peer, { call: "list_dir", path: target, limit: 2000 });
    if (reply.reply !== "listing") throw new Error("Unexpected answer");
    path.value = reply.listing.path;
    parent.value = reply.listing.parent;
    folders.value = reply.listing.entries.filter((e) => e.kind === "directory" && !e.hidden);
  } catch (err) {
    error.value = describeRemoteError(err, props.peer);
  } finally {
    loading.value = false;
  }
}

onMounted(async () => {
  try {
    const reply = await api.remoteCall(props.peer, { call: "roots" });
    if (reply.reply === "roots") roots.value = reply.roots;
  } catch (err) {
    error.value = describeRemoteError(err, props.peer);
  }
  const first = props.start ?? roots.value[0]?.path;
  if (first) await list(first);
});
</script>

<template>
  <div class="rounded-md border border-[var(--color-line)]">
    <div class="flex flex-wrap gap-1 border-b border-[var(--color-line)] px-2 py-1.5">
      <button
        v-for="root in roots"
        :key="root.path"
        type="button"
        class="rounded px-2 py-0.5 text-[12px] hover:bg-[var(--color-canvas)]"
        @click="list(root.path)"
      >
        {{ root.name }}
      </button>
    </div>
    <div class="flex items-center gap-2 px-2 py-1.5 text-[12px]">
      <button
        type="button"
        class="rounded px-1.5 disabled:opacity-40"
        :disabled="!parent || loading"
        aria-label="Up a folder"
        @click="parent && list(parent)"
      >
        ↑
      </button>
      <span class="mono min-w-0 flex-1 truncate text-[var(--color-muted)]" :title="path ?? ''">{{ path }}</span>
      <button
        type="button"
        class="shrink-0 rounded-md bg-[var(--color-accent)] px-2 py-0.5 text-[var(--color-accent-fg)] disabled:opacity-60"
        :disabled="!path || loading"
        @click="path && emit('pick', path)"
      >
        Use this folder
      </button>
    </div>
    <p v-if="error" class="px-2 pb-2 text-[12px] text-[var(--color-danger)]">{{ error }}</p>
    <ul class="max-h-56 overflow-auto border-t border-[var(--color-line)]">
      <li v-if="loading" class="px-3 py-1.5 text-[12px] text-[var(--color-muted)]">Loading…</li>
      <li v-else-if="!folders.length" class="px-3 py-1.5 text-[12px] text-[var(--color-muted)]">
        No folders here.
      </li>
      <li v-for="entry in folders" v-else :key="entry.path">
        <button
          type="button"
          class="flex w-full items-center gap-2 px-3 py-1 text-left hover:bg-[var(--color-canvas)] disabled:opacity-50"
          :disabled="!!entry.mount"
          :title="entry.mount ? 'Already syncing' : entry.path"
          @click="list(entry.path)"
        >
          <span aria-hidden="true">📁</span>
          <span class="truncate">{{ entry.name }}</span>
          <span v-if="entry.mount" class="ml-auto text-[11px] text-[var(--color-muted)]">syncing</span>
        </button>
      </li>
    </ul>
  </div>
</template>
