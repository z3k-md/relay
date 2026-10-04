<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import EmptyState from "../components/EmptyState.vue";
import ErrorBanner from "../components/ErrorBanner.vue";
import { api, RemoteCallError } from "../lib/api";
import type { DirEntry, DirListing, PeerView, RemoteRoot } from "../lib/types";

const peers = ref<PeerView[]>([]);
const device = ref<string | null>(null);
const roots = ref<RemoteRoot[]>([]);
/** The folder shown; `null` shows the device's roots. */
const listing = ref<DirListing | null>(null);
const showHidden = ref(false);
const loading = ref(false);
const error = ref<string | null>(null);

const browsable = computed(() => peers.value.filter((p) => p.connected && p.canManage));
const unavailable = computed(() => peers.value.filter((p) => !(p.connected && p.canManage)));
const entries = computed(() =>
  (listing.value?.entries ?? []).filter((entry) => showHidden.value || !entry.hidden),
);

/** Why a paired device cannot be browsed right now. */
function whyNot(peer: PeerView): string {
  if (!peer.connected) return "Offline";
  if (!peer.supportsRemote) return "Update Relay on it to browse it";
  return "Has not allowed this computer to manage it";
}

function describe(err: unknown, name: string): string {
  if (!(err instanceof RemoteCallError)) {
    return err instanceof Error ? err.message : String(err);
  }
  switch (err.code) {
    case "denied":
      return `${name} needs to allow this. ${err.message}`;
    case "forbidden":
      return `${name} has not allowed this computer to manage it. Turn it on in Peers on ${name}.`;
    case "offline":
      return `${name} is not connected.`;
    case "unsupported":
      return `${name} runs an older Relay. Update it to browse it.`;
    case "timeout":
      return `${name} did not answer in time. A permission prompt may be waiting on it.`;
    default:
      return err.message;
  }
}

async function run(action: () => Promise<void>) {
  const name = device.value ?? "";
  loading.value = true;
  error.value = null;
  try {
    await action();
  } catch (err) {
    error.value = describe(err, name);
  } finally {
    loading.value = false;
  }
}

async function choose(name: string) {
  device.value = name;
  listing.value = null;
  roots.value = [];
  await run(async () => {
    const reply = await api.remoteCall(name, { call: "roots" });
    if (reply.reply === "roots") roots.value = reply.roots;
  });
}

async function open(path: string) {
  const name = device.value;
  if (!name) return;
  await run(async () => {
    const reply = await api.remoteCall(name, { call: "list_dir", path });
    if (reply.reply === "listing") listing.value = reply.listing;
  });
}

async function loadMore() {
  const name = device.value;
  const current = listing.value;
  if (!name || !current || current.next_cursor == null) return;
  const cursor = current.next_cursor;
  await run(async () => {
    const reply = await api.remoteCall(name, { call: "list_dir", path: current.path, cursor });
    if (reply.reply === "listing") {
      listing.value = { ...reply.listing, entries: [...current.entries, ...reply.listing.entries] };
    }
  });
}

function up() {
  const parent = listing.value?.parent;
  if (parent) void open(parent);
  else listing.value = null;
}

function formatSize(bytes: number | null): string {
  if (bytes == null) return "";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value < 10 && unit > 0 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
}

function isFolder(entry: DirEntry): boolean {
  return entry.kind === "directory";
}

async function loadPeers() {
  try {
    peers.value = await api.listPeers();
    const first = browsable.value[0];
    if (first && !device.value) await choose(first.name);
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  }
}

onMounted(loadPeers);
</script>

<template>
  <div>
    <div class="mb-3 flex flex-wrap items-center justify-between gap-2">
      <h2 class="text-[15px] font-semibold">Browse</h2>
      <div v-if="browsable.length" class="flex items-center gap-2">
        <label class="text-[12px] text-[var(--color-muted)]" for="browse-device">Device</label>
        <select
          id="browse-device"
          :value="device ?? ''"
          class="rounded-md border border-[var(--color-line)] bg-[var(--color-canvas)] px-2 py-1"
          @change="choose(($event.target as HTMLSelectElement).value)"
        >
          <option v-for="peer in browsable" :key="peer.id" :value="peer.name">{{ peer.name }}</option>
        </select>
      </div>
    </div>
    <ErrorBanner :message="error" />

    <EmptyState
      v-if="!browsable.length"
      title="No device to browse"
      body="A paired device appears here once it is online and has allowed this computer to manage it. That choice is made while pairing, or later in Peers on that device."
    >
      <ul v-if="unavailable.length" class="mb-3 space-y-1 text-left text-[13px]">
        <li v-for="peer in unavailable" :key="peer.id">
          <span class="font-medium">{{ peer.name }}</span>
          <span class="text-[var(--color-muted)]"> — {{ whyNot(peer) }}</span>
        </li>
      </ul>
      <button
        type="button"
        class="rounded-md border border-[var(--color-line)] px-2.5 py-1"
        @click="loadPeers"
      >
        Check again
      </button>
    </EmptyState>

    <template v-else-if="device">
      <div class="mb-2 flex items-center gap-2">
        <button
          type="button"
          class="rounded-md border border-[var(--color-line)] px-2 py-0.5 text-[12px] disabled:opacity-50"
          :disabled="!listing || loading"
          @click="up"
        >
          Up
        </button>
        <p class="mono min-w-0 flex-1 truncate text-[12px] text-[var(--color-muted)]">
          {{ listing ? listing.path : `${device}: start here` }}
        </p>
        <label v-if="listing" class="flex shrink-0 items-center gap-1 text-[12px]">
          <input v-model="showHidden" type="checkbox" />
          Show hidden
        </label>
      </div>
      <p
        v-if="listing?.inside_mount"
        class="mb-2 rounded-md bg-[var(--color-panel)] px-2 py-1 text-[12px] text-[var(--color-muted)]"
      >
        Inside the synced folder {{ listing.inside_mount.space }}/{{ listing.inside_mount.mount }}.
      </p>

      <ul v-if="!listing" class="space-y-1">
        <li v-for="root in roots" :key="root.path">
          <button
            type="button"
            class="w-full rounded-md border border-[var(--color-line)] bg-[var(--color-panel)] px-3 py-2 text-left"
            :disabled="loading"
            @click="open(root.path)"
          >
            <span class="font-medium">{{ root.name }}</span>
            <span class="mono ml-2 text-[12px] text-[var(--color-muted)]">{{ root.path }}</span>
          </button>
        </li>
      </ul>

      <template v-else>
        <p v-if="!entries.length && !loading" class="text-[var(--color-muted)]">This folder is empty.</p>
        <ul class="divide-y divide-[var(--color-line)] rounded-md border border-[var(--color-line)]">
          <li v-for="entry in entries" :key="entry.path" class="flex items-center gap-2 px-3 py-1.5">
            <button
              v-if="isFolder(entry)"
              type="button"
              class="min-w-0 flex-1 truncate text-left font-medium"
              :disabled="loading"
              @click="open(entry.path)"
            >
              📁 {{ entry.name }}
            </button>
            <span v-else class="min-w-0 flex-1 truncate">📄 {{ entry.name }}</span>
            <span
              v-if="entry.mount"
              class="shrink-0 rounded-full bg-emerald-100 px-2 text-[11px] text-emerald-800 dark:bg-emerald-950 dark:text-emerald-200"
            >
              Synced · {{ entry.mount.space }}
            </span>
            <span
              v-else-if="entry.contains_mount"
              class="shrink-0 rounded-full border border-[var(--color-line)] px-2 text-[11px] text-[var(--color-muted)]"
            >
              Contains synced folder
            </span>
            <span
              v-if="entry.cloud_only"
              class="shrink-0 rounded-full border border-[var(--color-line)] px-2 text-[11px] text-[var(--color-muted)]"
              title="A cloud placeholder: syncing it downloads it"
            >
              Cloud
            </span>
            <span class="w-16 shrink-0 text-right text-[12px] text-[var(--color-muted)]">
              {{ formatSize(entry.size) }}
            </span>
          </li>
        </ul>
        <div v-if="listing.next_cursor != null" class="mt-2 flex justify-center">
          <button
            type="button"
            class="rounded-md border border-[var(--color-line)] px-2.5 py-1"
            :disabled="loading"
            @click="loadMore"
          >
            Load more ({{ listing.entries.length }} of {{ listing.total }})
          </button>
        </div>
      </template>
      <p v-if="loading" class="mt-2 text-[12px] text-[var(--color-muted)]">Loading…</p>
    </template>
  </div>
</template>
