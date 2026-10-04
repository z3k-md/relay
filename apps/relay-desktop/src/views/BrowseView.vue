<script setup lang="ts">
import { confirm } from "@tauri-apps/plugin-dialog";
import { computed, onMounted, ref } from "vue";
import EmptyState from "../components/EmptyState.vue";
import ErrorBanner from "../components/ErrorBanner.vue";
import PairFolderDialog from "../components/PairFolderDialog.vue";
import { api } from "../lib/api";
import { useRemoteFolders } from "../lib/remoteFolders";
import { describeRemoteError } from "../lib/remoteFolders";
import type { DirEntry, PeerView, QuickOpen } from "../lib/types";

const peers = ref<PeerView[]>([]);
const showHidden = ref(false);
const { device, roots, listing, loading, error, choose, open, loadMore, up } = useRemoteFolders();
/** The remote folder a "Sync…" dialog is open for. */
const pairing = ref<{ path: string; name: string } | null>(null);
/** The remote file being fetched to open. */
const opening = ref<string | null>(null);
const quickOpens = ref<QuickOpen[]>([]);
const notice = ref<string | null>(null);

/** Above this, ask before downloading a file just to open it. */
const LARGE_FILE = 1024 ** 3;

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

function syncFolder(path: string, name: string) {
  pairing.value = { path, name };
}

/** A folder's own name, for display. Paths are otherwise never split. */
function lastName(path: string): string {
  return path.split(/[\\/]/).filter(Boolean).pop() ?? path;
}

function syncedSomewhere(entry: DirEntry): boolean {
  return !!entry.mount || entry.contains_mount;
}

async function openFile(entry: DirEntry) {
  const name = device.value;
  if (!name || opening.value) return;
  if ((entry.size ?? 0) > LARGE_FILE) {
    const ok = await confirm(
      `${entry.name} is ${formatSize(entry.size)}. It downloads fully before it opens.`,
      { title: "Download a large file?", kind: "warning" },
    );
    if (!ok) return;
  }
  opening.value = entry.path;
  error.value = null;
  notice.value = null;
  try {
    await api.openRemoteFile(name, entry.path);
    await loadQuickOpens();
  } catch (err) {
    error.value = describeRemoteError(err, name);
  } finally {
    opening.value = null;
  }
}

async function loadQuickOpens() {
  try {
    quickOpens.value = await api.listQuickOpens();
  } catch {
    quickOpens.value = [];
  }
}

async function removeQuickOpen(space: string) {
  error.value = null;
  try {
    notice.value = (await api.removeQuickOpen(space)) ?? null;
    await loadQuickOpens();
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  }
}

async function afterPair() {
  pairing.value = null;
  if (listing.value) await open(listing.value.path);
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

onMounted(() => Promise.all([loadPeers(), loadQuickOpens()]));
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
      <div v-else-if="listing && listing.parent" class="mb-2 flex justify-end">
        <button
          type="button"
          class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[12px] text-[var(--color-accent-fg)]"
          @click="syncFolder(listing.path, lastName(listing.path))"
        >
          Sync this folder…
        </button>
      </div>

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
            <button
              v-else
              type="button"
              class="min-w-0 flex-1 truncate text-left"
              :disabled="!!opening"
              :title="`Open ${entry.name} here`"
              @click="openFile(entry)"
            >
              📄 {{ entry.name }}
              <span v-if="opening === entry.path" class="text-[12px] text-[var(--color-muted)]">
                Opening…
              </span>
            </button>
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
            <button
              v-if="isFolder(entry) && !syncedSomewhere(entry) && !listing.inside_mount"
              type="button"
              class="shrink-0 rounded-md border border-[var(--color-line)] px-2 py-0.5 text-[12px]"
              @click="syncFolder(entry.path, entry.name)"
            >
              Sync…
            </button>
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

    <p v-if="notice" class="mt-3 text-[12px] text-[var(--color-muted)]">{{ notice }}</p>
    <section v-if="quickOpens.length" class="mt-6">
      <h3 class="mb-1 text-[13px] font-semibold">Opened from other devices</h3>
      <p class="mb-2 text-[12px] text-[var(--color-muted)]">
        Folders synced online-only so their files could open here. Removing one keeps the files
        already here.
      </p>
      <ul class="divide-y divide-[var(--color-line)] rounded-md border border-[var(--color-line)]">
        <li v-for="q in quickOpens" :key="q.space" class="flex items-center gap-2 px-3 py-1.5">
          <div class="min-w-0 flex-1">
            <p class="truncate text-[13px]">{{ q.peer }}: <span class="mono">{{ q.folder }}</span></p>
            <p class="mono truncate text-[12px] text-[var(--color-muted)]">{{ q.local_path ?? "" }}</p>
          </div>
          <button
            type="button"
            class="shrink-0 rounded-md border border-[var(--color-line)] px-2 py-0.5 text-[12px]"
            @click="removeQuickOpen(q.space)"
          >
            Remove
          </button>
        </li>
      </ul>
    </section>

    <PairFolderDialog
      v-if="pairing && device"
      :source-device="device"
      :source-path="pairing.path"
      :source-name="pairing.name"
      :peers="browsable"
      @close="pairing = null"
      @paired="afterPair"
    />
  </div>
</template>
