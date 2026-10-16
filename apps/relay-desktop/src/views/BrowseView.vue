<script lang="ts">
import { useRemoteFolders } from "../lib/remoteFolders";

/** Kept across visits to the page, so coming back resumes where you were. */
const browser = useRemoteFolders({ sizes: true });
/** Name order: case does not matter, and 2 comes before 10. */
const collator = new Intl.Collator(undefined, { sensitivity: "base", numeric: true });
</script>

<script setup lang="ts">
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { confirm } from "@tauri-apps/plugin-dialog";
import {
  computed,
  nextTick,
  onActivated,
  onDeactivated,
  onMounted,
  onUnmounted,
  ref,
  watch,
} from "vue";
import EmptyState from "../components/EmptyState.vue";
import ErrorBanner from "../components/ErrorBanner.vue";
import PairFolderDialog from "../components/PairFolderDialog.vue";
import { api, errorText } from "../lib/api";
import { formatSize } from "../lib/format";
import { describeRemoteError } from "../lib/remoteFolders";
import type { ActivityItem, DirEntry, DirListing, PeerView, QuickOpen } from "../lib/types";

const peers = ref<PeerView[]>([]);
const showHidden = ref(false);
const {
  device,
  roots,
  listing,
  loading,
  error,
  pending,
  sizes,
  sizesDone,
  sizesSupported,
  sizesFailed,
  canBack,
  canForward,
  choose,
  open,
  back,
  forward,
  up,
  home,
  resume,
  refresh,
  loadMore,
  stopSizes,
} = browser;
/** The remote folder a "Sync…" dialog is open for. */
const pairing = ref<{ path: string; name: string } | null>(null);
/** The remote file being fetched to open. */
const opening = ref<string | null>(null);
const quickOpens = ref<QuickOpen[]>([]);
const notice = ref<string | null>(null);

/** Above this, ask before downloading a file just to open it. */
const LARGE_FILE = 1024 ** 3;
/** The most a read-only copy fetches (relay-core READ_COPY_MAX). */
const READ_COPY_MAX = 256 * 1024 ** 2;
/** The file last opened as a read-only copy, for the "sync to edit" offer. */
const copied = ref<DirEntry | null>(null);

type SortKey = "name" | "modified" | "size";
const sortKey = ref<SortKey>("name");
const sortDesc = ref(false);

/** The path bar is a text field while typing a path. */
const editing = ref(false);
const typedPath = ref("");
const pathInput = ref<HTMLInputElement | null>(null);
const crumbBar = ref<HTMLElement | null>(null);
let stopActivity: UnlistenFn | undefined;
/** Set on unmount, so a listener that resolves after it is dropped at once. */
let gone = false;

const browsable = computed(() => peers.value.filter((p) => p.connected && p.canManage));
const unavailable = computed(() => peers.value.filter((p) => !(p.connected && p.canManage)));

function isFolder(entry: DirEntry): boolean {
  return entry.kind === "directory";
}

/** Space on disk: allocated for files, counted for folders; null if unknown yet. */
function diskSize(entry: DirEntry): number | null {
  if (isFolder(entry)) return sizes.value.get(entry.path)?.bytes ?? null;
  return entry.disk_size ?? entry.size;
}

/** Folder sizes are still being counted, so there is nothing to sort folders by yet. */
const counting = computed(() => sizesSupported.value && !sizesFailed.value && !sizesDone.value);

const entries = computed(() => {
  const shown = (listing.value?.entries ?? []).filter((entry) => showHidden.value || !entry.hidden);
  const dir = sortDesc.value ? -1 : 1;
  const foldersFirst = (a: DirEntry, b: DirEntry) => Number(isFolder(b)) - Number(isFolder(a));
  const byName = (a: DirEntry, b: DirEntry) => collator.compare(a.name, b.name);
  return [...shown].sort((a, b) => {
    switch (sortKey.value) {
      case "name":
        return foldersFirst(a, b) || dir * byName(a, b);
      case "modified":
        return foldersFirst(a, b) || dir * ((a.modified_ms ?? 0) - (b.modified_ms ?? 0)) || byName(a, b);
      case "size":
        // Mixed, so whatever takes the most space comes first. Folders keep
        // their name order while being counted, so rows do not jump around.
        if (counting.value && (isFolder(a) || isFolder(b))) return foldersFirst(a, b) || byName(a, b);
        return dir * ((diskSize(a) ?? -1) - (diskSize(b) ?? -1)) || byName(a, b);
    }
  });
});

function setSort(key: SortKey) {
  if (sortKey.value === key) {
    sortDesc.value = !sortDesc.value;
  } else {
    sortKey.value = key;
    sortDesc.value = key !== "name";
  }
}

function sortMark(key: SortKey): string {
  if (sortKey.value !== key) return "";
  return sortDesc.value ? "↓" : "↑";
}

/**
 * The path bar: the device, then the folders down to this one. Anything above
 * a starting place (Home, a drive) shows as that place's name. Paths come from
 * the device; they are only compared, never split.
 */
const crumbs = computed(() => {
  const list: { name: string; path: string | null }[] = [{ name: device.value ?? "", path: null }];
  const shown = listing.value;
  if (!shown) return list;
  const above = shown.ancestors?.length ? shown.ancestors : [{ name: shown.path, path: shown.path }];
  let start = 0;
  let rootName: string | null = null;
  for (let i = above.length - 1; i >= 0; i--) {
    const root = roots.value.find((r) => r.path === above[i].path);
    if (root) {
      start = i;
      rootName = root.name;
      break;
    }
  }
  above.slice(start).forEach((folder, i) => {
    list.push({ name: i === 0 && rootName ? rootName : folder.name, path: folder.path });
  });
  return list;
});

// On the path, not the crumbs: a background refresh makes new crumbs too.
watch(
  () => listing.value?.path,
  async () => {
    await nextTick();
    const bar = crumbBar.value;
    if (bar) bar.scrollLeft = bar.scrollWidth;
  },
);

const summary = computed(() => {
  const shown = listing.value;
  if (!shown) return "";
  const folders = entries.value.filter(isFolder);
  const files = entries.value.length - folders.length;
  const parts = [];
  if (shown.next_cursor != null) {
    // More pages: the device's count, not how many are loaded.
    parts.push(`${shown.total.toLocaleString()} items (${entries.value.length.toLocaleString()} shown)`);
  } else {
    if (folders.length) parts.push(`${folders.length} ${folders.length === 1 ? "folder" : "folders"}`);
    if (files) parts.push(`${files} ${files === 1 ? "file" : "files"}`);
  }
  const counted = folders.every((f) => sizes.value.has(f.path));
  if (!folders.length || (sizesSupported.value && counted)) {
    const total = entries.value.reduce((sum, e) => sum + (diskSize(e) ?? 0), 0);
    const partial = shown.next_cursor != null || (folders.length > 0 && !sizesDone.value);
    parts.push(`${formatSize(total)} on disk${partial ? " so far" : ""}`);
  }
  return parts.join(" · ");
});

/** Why a paired device cannot be browsed right now. */
function whyNot(peer: PeerView): string {
  if (!peer.connected) return "Offline";
  if (!peer.supportsRemote) return "Update Relay on it to browse it";
  return "Has not allowed this computer to manage it";
}

function syncFolder(path: string, name: string) {
  pairing.value = { path, name };
}

/** A folder's own name. Older devices send no ancestors; split only then. */
function folderName(shown: DirListing): string {
  const above = shown.ancestors ?? [];
  if (above.length) return above[above.length - 1].name;
  return shown.path.split(/[\\/]/).filter(Boolean).pop() ?? shown.path;
}

function syncedSomewhere(entry: DirEntry): boolean {
  return !!entry.mount || entry.contains_mount;
}

async function openFile(entry: DirEntry) {
  const name = device.value;
  if (!name || opening.value) return;
  error.value = null;
  notice.value = null;
  try {
    if ((entry.size ?? 0) > LARGE_FILE) {
      const ok = await confirm(
        `${entry.name} is ${formatSize(entry.size)}. It downloads fully before it opens.`,
        { title: "Download a large file?", kind: "warning" },
      );
      if (!ok) return;
    }
    opening.value = entry.path;
    await api.openRemoteFile(name, entry.path);
    await loadQuickOpens();
  } catch (err) {
    error.value = describeRemoteError(err, name);
  } finally {
    opening.value = null;
  }
}

async function copyFile(entry: DirEntry) {
  const name = device.value;
  if (!name || opening.value) return;
  opening.value = entry.path;
  error.value = null;
  notice.value = null;
  copied.value = null;
  try {
    await api.openRemoteFile(name, entry.path, true);
    copied.value = entry;
  } catch (err) {
    error.value = describeRemoteError(err, name);
  } finally {
    opening.value = null;
  }
}

async function syncToEdit() {
  const entry = copied.value;
  copied.value = null;
  if (entry) await openFile(entry);
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
    error.value = errorText(err);
  }
}

async function afterPair() {
  pairing.value = null;
  await refresh();
}

const dateFormat = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" });

function formatModified(ms: number | null): string {
  return ms == null ? "" : dateFormat.format(new Date(ms));
}

function sizeTitle(entry: DirEntry): string {
  if (isFolder(entry)) {
    const size = sizes.value.get(entry.path);
    if (!size) {
      if (!sizesSupported.value) return "Update Relay on that device to see folder sizes";
      return sizesFailed.value ?? "Counting…";
    }
    const files = `${size.files.toLocaleString()} ${size.files === 1 ? "file" : "files"}`;
    return `${formatSize(size.bytes)} on disk, ${files}${size.done ? "" : " so far (counting)"}`;
  }
  const onDisk = entry.disk_size;
  return onDisk == null
    ? `${formatSize(entry.size)}`
    : `${formatSize(onDisk)} on disk (${(entry.size ?? 0).toLocaleString()} bytes)`;
}

function startEditing() {
  typedPath.value = listing.value?.path ?? "";
  editing.value = true;
  void nextTick(() => pathInput.value?.select());
}

function submitPath() {
  const path = typedPath.value.trim();
  editing.value = false;
  if (path) void open(path);
}

/** Mouse back/forward buttons (3 and 4), whichever the system sends. */
function onMouse(event: MouseEvent) {
  if (event.button !== 3 && event.button !== 4) return;
  // Keep the webview from treating it as its own navigation.
  event.preventDefault();
  if (event.type !== "mouseup" || pairing.value || !device.value) return;
  if (event.button === 3) void back();
  else void forward();
}

function onKey(event: KeyboardEvent) {
  if (pairing.value || editing.value || !device.value) return;
  const target = event.target as HTMLElement | null;
  if (target?.closest("input, textarea, select, [contenteditable]")) return;
  const mod = event.metaKey || event.ctrlKey;
  const { key, altKey, metaKey } = event;
  if ((altKey && key === "ArrowLeft") || (metaKey && key === "[") || key === "BrowserBack") {
    void back();
  } else if ((altKey && key === "ArrowRight") || (metaKey && key === "]") || key === "BrowserForward") {
    void forward();
  } else if ((altKey || metaKey) && key === "ArrowUp") {
    void up();
  } else if (key === "F5" || key === "BrowserRefresh" || (mod && key.toLowerCase() === "r")) {
    void refresh();
  } else if (mod && key.toLowerCase() === "l") {
    startEditing();
  } else {
    return;
  }
  event.preventDefault();
}

async function loadPeers() {
  try {
    const before = browsable.value;
    peers.value = await api.listPeers();
    const current = device.value;
    if (current && browsable.value.some((p) => p.name === current)) {
      // Only a device that just became browsable is listed again: resuming
      // on every peer blip would drop a navigation in flight.
      if (!before.some((p) => p.name === current)) await resume();
    } else {
      const first = browsable.value[0];
      if (first) await choose(first.name);
    }
  } catch (err) {
    error.value = errorText(err);
  }
}

onMounted(async () => {
  const off = await listen<ActivityItem>("relay://activity", (event) => {
    if (event.payload.kind === "peerConnected" || event.payload.kind === "peerDisconnected") {
      void loadPeers();
    }
  });
  if (gone) off();
  else stopActivity = off;
});

// Kept alive across tab switches: keys and mouse buttons only act while the
// page shows, and a revisit refreshes in place, keeping the open folder.
onActivated(async () => {
  window.addEventListener("mousedown", onMouse);
  window.addEventListener("mouseup", onMouse);
  window.addEventListener("keydown", onKey);
  await Promise.all([loadPeers(), loadQuickOpens()]);
});

onDeactivated(() => {
  window.removeEventListener("mousedown", onMouse);
  window.removeEventListener("mouseup", onMouse);
  window.removeEventListener("keydown", onKey);
});

onUnmounted(() => {
  gone = true;
  stopActivity?.();
  stopSizes();
});
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
      <div class="mb-1.5 flex items-center gap-2">
        <div class="flex shrink-0 overflow-hidden rounded-md border border-[var(--color-line)] bg-[var(--color-panel)]">
          <button
            type="button"
            class="nav-button"
            :disabled="!canBack"
            title="Back (Alt+←, mouse back button)"
            aria-label="Back"
            @click="back"
          >
            <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M10 3 5 8l5 5" /></svg>
          </button>
          <button
            type="button"
            class="nav-button"
            :disabled="!canForward"
            title="Forward (Alt+→, mouse forward button)"
            aria-label="Forward"
            @click="forward"
          >
            <svg viewBox="0 0 16 16" aria-hidden="true"><path d="m6 3 5 5-5 5" /></svg>
          </button>
          <button
            type="button"
            class="nav-button"
            :disabled="!listing"
            title="Up a folder (Alt+↑)"
            aria-label="Up"
            @click="up"
          >
            <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M8 13V3M3.5 7.5 8 3l4.5 4.5" /></svg>
          </button>
          <button
            type="button"
            class="nav-button"
            title="Refresh (F5)"
            aria-label="Refresh"
            @click="refresh"
          >
            <svg viewBox="0 0 16 16" aria-hidden="true" :class="{ 'animate-spin': loading }">
              <path d="M13 8a5 5 0 1 1-1.5-3.55M13 2.5V5h-2.5" />
            </svg>
          </button>
        </div>

        <div
          class="relative flex h-8 min-w-0 flex-1 items-center overflow-hidden rounded-md border border-[var(--color-line)] bg-[var(--color-panel)]"
          @click.self="startEditing"
        >
          <input
            v-if="editing"
            ref="pathInput"
            v-model="typedPath"
            class="mono h-full w-full bg-transparent px-2 text-[12px] outline-none"
            aria-label="Folder path"
            spellcheck="false"
            @keydown.enter.prevent="submitPath"
            @keydown.esc.prevent="editing = false"
            @blur="editing = false"
          />
          <nav
            v-else
            ref="crumbBar"
            class="no-scrollbar flex h-full min-w-0 flex-1 items-center overflow-x-auto px-1"
            aria-label="Folder path"
            @click.self="startEditing"
          >
            <template v-for="(crumb, i) in crumbs" :key="crumb.path ?? '\u0000device'">
              <span v-if="i" class="shrink-0 px-0.5 text-[var(--color-muted)]" aria-hidden="true">›</span>
              <button
                type="button"
                class="shrink-0 whitespace-nowrap rounded px-1.5 py-0.5 hover:bg-[var(--color-canvas)]"
                :class="i === crumbs.length - 1 ? 'font-medium' : 'text-[var(--color-muted)]'"
                :title="crumb.path ?? `${device}: starting places`"
                :aria-current="i === crumbs.length - 1 ? 'location' : undefined"
                @click="crumb.path ? open(crumb.path) : home()"
              >
                {{ crumb.name }}
              </button>
            </template>
            <span class="min-w-6 flex-1 self-stretch" title="Type a path (Ctrl+L)" @click="startEditing" />
          </nav>
          <div v-if="loading" class="loading-bar" aria-hidden="true" />
        </div>
      </div>

      <div class="mb-2 flex min-h-7 flex-wrap items-center justify-between gap-2 text-[12px]">
        <span class="text-[var(--color-muted)]">
          <template v-if="listing?.inside_mount">
            Inside the synced folder {{ listing.inside_mount.space }}/{{ listing.inside_mount.mount }}.
          </template>
          <template v-if="summary">{{ listing?.inside_mount ? " " : "" }}{{ summary }}</template>
        </span>
        <span class="flex items-center gap-3">
          <label v-if="listing" class="flex shrink-0 items-center gap-1">
            <input v-model="showHidden" type="checkbox" />
            Show hidden
          </label>
          <button
            v-if="listing && listing.parent && !listing.inside_mount"
            type="button"
            class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)]"
            @click="syncFolder(listing.path, folderName(listing))"
          >
            Sync this folder…
          </button>
        </span>
      </div>

      <ul v-if="!listing" class="space-y-1" :class="{ 'opacity-60': pending }">
        <li v-for="root in roots" :key="root.path">
          <button
            type="button"
            class="w-full rounded-md border border-[var(--color-line)] bg-[var(--color-panel)] px-3 py-2 text-left hover:border-[var(--color-muted)]"
            @click="open(root.path)"
          >
            <span class="font-medium">{{ root.name }}</span>
            <span class="mono ml-2 text-[12px] text-[var(--color-muted)]">{{ root.path }}</span>
          </button>
        </li>
      </ul>

      <template v-else>
        <div
          class="overflow-hidden rounded-md border border-[var(--color-line)] transition-opacity"
          :class="{ 'opacity-60': pending }"
        >
          <div
            class="flex items-center gap-2 border-b border-[var(--color-line)] bg-[var(--color-panel)] px-3 py-1 text-[12px] text-[var(--color-muted)]"
          >
            <button type="button" class="min-w-0 flex-1 text-left" @click="setSort('name')">
              Name {{ sortMark("name") }}
            </button>
            <button type="button" class="hidden w-36 shrink-0 text-left md:block" @click="setSort('modified')">
              Modified {{ sortMark("modified") }}
            </button>
            <button
              type="button"
              class="w-20 shrink-0 text-right"
              title="Space on disk"
              @click="setSort('size')"
            >
              {{ sortMark("size") }} Size
            </button>
            <span class="w-20 shrink-0" aria-hidden="true"></span>
          </div>
          <p v-if="!entries.length && !loading" class="px-3 py-3 text-[var(--color-muted)]">
            This folder is empty.
          </p>
          <ul class="divide-y divide-[var(--color-line)]">
            <li
              v-for="entry in entries"
              :key="entry.path"
              class="browse-row flex min-h-9 items-center gap-2 px-3 py-1 hover:bg-[var(--color-panel)]"
            >
              <button
                v-if="isFolder(entry)"
                type="button"
                class="min-w-0 flex-1 truncate text-left font-medium"
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
              <span class="hidden w-36 shrink-0 truncate text-[12px] text-[var(--color-muted)] md:block">
                {{ formatModified(entry.modified_ms) }}
              </span>
              <span
                class="w-20 shrink-0 text-right text-[12px] tabular-nums"
                :class="
                  isFolder(entry) && counting && !sizes.get(entry.path)?.done
                    ? 'animate-pulse text-[var(--color-muted)]'
                    : 'text-[var(--color-muted)]'
                "
                :title="sizeTitle(entry)"
              >
                <template v-if="isFolder(entry)">
                  {{ sizes.has(entry.path) ? formatSize(sizes.get(entry.path)?.bytes) : counting ? "…" : "—" }}
                </template>
                <template v-else>{{ formatSize(diskSize(entry)) }}</template>
              </span>
              <span class="flex w-20 shrink-0 justify-end">
                <button
                  v-if="isFolder(entry) && !syncedSomewhere(entry) && !listing.inside_mount"
                  type="button"
                  class="rounded-md border border-[var(--color-line)] px-2 py-0.5 text-[12px]"
                  :title="`Sync ${entry.name} to a folder on this computer`"
                  @click="syncFolder(entry.path, entry.name)"
                >
                  Sync
                </button>
                <button
                  v-else-if="!isFolder(entry) && listing.inside_mount && (entry.size ?? 0) <= READ_COPY_MAX"
                  type="button"
                  class="rounded-md border border-[var(--color-line)] px-2 py-0.5 text-[12px]"
                  :disabled="!!opening"
                  title="Open a copy that sets up nothing; edits stay on this computer"
                  @click="copyFile(entry)"
                >
                  Read-only
                </button>
              </span>
            </li>
          </ul>
        </div>
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
    </template>

    <div
      v-if="copied && device"
      class="mt-3 flex flex-wrap items-center justify-between gap-2 rounded-md border border-[var(--color-line)] bg-[var(--color-panel)] px-3 py-2 text-[13px]"
    >
      <span>
        Opened a read-only copy of {{ copied.name }}. Edits to it stay on this computer.
      </span>
      <span class="flex shrink-0 gap-2">
        <button
          type="button"
          class="rounded-md bg-[var(--color-accent)] px-2 py-0.5 text-[12px] text-[var(--color-accent-fg)]"
          @click="syncToEdit"
        >
          Sync this folder to edit
        </button>
        <button type="button" class="text-[12px] text-[var(--color-muted)]" @click="copied = null">
          Dismiss
        </button>
      </span>
    </div>
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

<style scoped>
.nav-button {
  display: grid;
  place-items: center;
  width: 2rem;
  height: 2rem;
  color: var(--color-ink);
}

.nav-button + .nav-button {
  border-left: 1px solid var(--color-line);
}

.nav-button:hover:not(:disabled) {
  background: var(--color-canvas);
}

.nav-button:disabled {
  color: var(--color-muted);
  opacity: 0.45;
}

.nav-button svg {
  width: 1rem;
  height: 1rem;
  fill: none;
  stroke: currentColor;
  stroke-width: 1.6;
  stroke-linecap: round;
  stroke-linejoin: round;
}

.no-scrollbar {
  scrollbar-width: none;
}

.no-scrollbar::-webkit-scrollbar {
  display: none;
}

.loading-bar {
  position: absolute;
  left: 0;
  right: 0;
  bottom: 0;
  height: 2px;
  overflow: hidden;
}

.loading-bar::after {
  content: "";
  position: absolute;
  inset: 0;
  width: 30%;
  background: var(--color-accent);
  animation: loading-slide 1s ease-in-out infinite;
}

@keyframes loading-slide {
  from {
    transform: translateX(-100%);
  }
  to {
    transform: translateX(340%);
  }
}

/* Long folders: rows off screen skip layout and paint. */
.browse-row {
  content-visibility: auto;
  contain-intrinsic-size: auto 2.25rem;
}
</style>
