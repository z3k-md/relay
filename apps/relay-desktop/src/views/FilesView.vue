<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import EmptyState from "../components/EmptyState.vue";
import ErrorBanner from "../components/ErrorBanner.vue";
import { api, errorText } from "../lib/api";
import { formatSize } from "../lib/format";
import type { FileRow, FolderView, SpaceView, TransferLive } from "../lib/types";

const props = defineProps<{
  transfers?: TransferLive[];
}>();

type Mount = { space: string; mount: string };

const spaces = ref<SpaceView[]>([]);
const current = ref<Mount | null>(null);
const folder = ref<FolderView | null>(null);
/** Paths with a download or open in flight. */
const working = ref(new Set<string>());
const busy = ref(false);
const error = ref<string | null>(null);
const notice = ref<string | null>(null);
/** Bumped on every folder opened; a late listing for an older one is dropped. */
let generation = 0;

const mounts = computed<Mount[]>(() =>
  spaces.value.flatMap((space) =>
    space.mounts.filter((m) => m.attached).map((m) => ({ space: space.name, mount: m.name })),
  ),
);

const crumbs = computed(() => {
  const path = folder.value?.path ?? "";
  const parts = path ? path.split("/") : [];
  return parts.map((name, i) => ({ name, path: parts.slice(0, i + 1).join("/") }));
});

/** What a folder holds on this device, as the user chose it. */
const folderChoice = computed(() => {
  const view = folder.value;
  if (!view) return "inherit";
  if (view.chosen_here === "full") return "full";
  if (view.chosen_here === "demand") return "demand";
  return "inherit";
});

function mountKey(m: Mount): string {
  return `${m.space}/${m.mount}`;
}

async function open(path: string) {
  const m = current.value;
  if (!m) return;
  const token = ++generation;
  error.value = null;
  try {
    const next = await api.listFolder(m.space, m.mount, path);
    if (token === generation) folder.value = next;
  } catch (err) {
    if (token === generation) error.value = errorText(err);
  }
}

async function chooseMount(key: string) {
  current.value = mounts.value.find((m) => mountKey(m) === key) ?? null;
  await open("");
}

async function withPath(path: string, action: () => Promise<unknown>) {
  working.value = new Set(working.value).add(path);
  error.value = null;
  notice.value = null;
  try {
    await action();
  } catch (err) {
    error.value = errorText(err);
  } finally {
    const next = new Set(working.value);
    next.delete(path);
    working.value = next;
    await open(folder.value?.path ?? "");
  }
}

function openRow(row: FileRow) {
  const m = current.value;
  if (!m) return;
  if (row.kind === "directory") {
    void open(row.path);
    return;
  }
  void withPath(row.path, () => api.openFile(m.space, m.mount, row.path));
}

function download(row: FileRow) {
  const m = current.value;
  if (m) void withPath(row.path, () => api.downloadFile(m.space, m.mount, row.path));
}

function free(path: string) {
  const m = current.value;
  if (!m) return;
  void withPath(path, async () => {
    const freed = await api.freeUpSpace(m.space, m.mount, path);
    notice.value = freed === 1 ? "Freed 1 file." : `Freed ${freed} files.`;
  });
}

async function setChoice(choice: string) {
  const m = current.value;
  const view = folder.value;
  if (!m || !view) return;
  busy.value = true;
  error.value = null;
  try {
    await api.setFolderMode(m.space, m.mount, view.path, choice === "inherit" ? null : choice);
    await open(view.path);
  } catch (err) {
    error.value = errorText(err);
  } finally {
    busy.value = false;
  }
}

/** Live bytes for a file being received, from the app's transfer feed. */
function progress(row: FileRow): string | null {
  const m = current.value;
  const live = (props.transfers ?? []).find(
    (t) =>
      t.direction === "receive" &&
      t.space === m?.space &&
      t.currentPath != null &&
      (t.currentPath === row.path || t.currentPath.endsWith(`/${row.path}`)),
  );
  if (!live || !live.bytesTotal) return null;
  return `${Math.round((live.bytesDone / live.bytesTotal) * 100)}%`;
}

function stateLabel(row: FileRow): string {
  if (working.value.has(row.path)) return progress(row) ?? "Downloading…";
  switch (row.state) {
    case "local":
      return row.mode === "demand" ? "On this device" : "";
    case "online_only":
      return "Online only";
    case "metadata_only":
      return "Not stored here";
    case "pending":
      return "Waiting to sync";
    case "stored":
      return "Kept in backup store";
  }
}

onMounted(async () => {
  try {
    spaces.value = await api.listSpaces();
    const first = mounts.value[0];
    if (first) await chooseMount(mountKey(first));
  } catch (err) {
    error.value = errorText(err);
  }
});
</script>

<template>
  <div>
    <div class="mb-3 flex flex-wrap items-center justify-between gap-2">
      <h2 class="text-[15px] font-semibold">Files</h2>
      <select
        v-if="mounts.length"
        :value="current ? mountKey(current) : ''"
        class="rounded-md border border-[var(--color-line)] bg-[var(--color-canvas)] px-2 py-1"
        @change="chooseMount(($event.target as HTMLSelectElement).value)"
      >
        <option v-for="m in mounts" :key="mountKey(m)" :value="mountKey(m)">
          {{ m.space }} / {{ m.mount }}
        </option>
      </select>
    </div>
    <ErrorBanner :message="error" />
    <p v-if="notice" class="mb-2 text-[12px] text-[var(--color-muted)]">{{ notice }}</p>

    <EmptyState
      v-if="!mounts.length"
      title="No synced folders on this computer"
      body="Add a folder to a space, or join one a peer shared, and its files show up here."
    />

    <template v-else-if="folder">
      <div class="mb-2 flex flex-wrap items-center gap-1 text-[13px]">
        <button type="button" class="underline-offset-2 hover:underline" @click="open('')">
          {{ folder.mount }}
        </button>
        <template v-for="crumb in crumbs" :key="crumb.path">
          <span class="text-[var(--color-muted)]">/</span>
          <button type="button" class="underline-offset-2 hover:underline" @click="open(crumb.path)">
            {{ crumb.name }}
          </button>
        </template>
      </div>

      <div
        class="mb-2 flex flex-wrap items-center justify-between gap-2 rounded-md bg-[var(--color-panel)] px-3 py-2"
      >
        <label class="flex items-center gap-2 text-[13px]">
          <span class="text-[var(--color-muted)]">This folder on this computer</span>
          <select
            :value="folderChoice"
            :disabled="busy"
            class="rounded-md border border-[var(--color-line)] bg-[var(--color-canvas)] px-2 py-0.5"
            @change="setChoice(($event.target as HTMLSelectElement).value)"
          >
            <option value="inherit">
              Same as {{ folder.path ? "the folder above" : "the space" }} ({{
                folder.mode === "demand" ? "online only" : "keep"
              }})
            </option>
            <option value="full">Always keep on this computer</option>
            <option value="demand">Online only</option>
          </select>
        </label>
        <button
          v-if="folder.mode === 'demand'"
          type="button"
          class="rounded-md border border-[var(--color-line)] px-2 py-0.5 text-[12px]"
          :disabled="working.has(folder.path)"
          @click="free(folder.path)"
        >
          Free up space
        </button>
      </div>

      <p v-if="!folder.entries.length" class="text-[var(--color-muted)]">This folder is empty.</p>
      <ul v-else class="divide-y divide-[var(--color-line)] rounded-md border border-[var(--color-line)]">
        <li v-for="row in folder.entries" :key="row.path" class="flex items-center gap-2 px-3 py-1.5">
          <button
            type="button"
            class="min-w-0 flex-1 truncate text-left"
            :class="row.kind === 'directory' ? 'font-medium' : ''"
            :disabled="working.has(row.path) || row.state === 'metadata_only' || row.state === 'stored'"
            @click="openRow(row)"
          >
            {{ row.kind === "directory" ? "📁" : row.state === "local" ? "📄" : "☁️" }}
            {{ row.name }}
            <span v-if="row.conflict_copy" class="text-[11px] text-[var(--color-danger)]">conflict copy</span>
          </button>
          <span class="shrink-0 text-[12px] text-[var(--color-muted)]">{{ stateLabel(row) }}</span>
          <button
            v-if="row.kind !== 'directory' && row.state === 'online_only' && !working.has(row.path)"
            type="button"
            class="shrink-0 rounded-md border border-[var(--color-line)] px-2 py-0.5 text-[12px]"
            @click="download(row)"
          >
            Download
          </button>
          <button
            v-if="row.kind !== 'directory' && row.state === 'local' && row.mode === 'demand'"
            type="button"
            class="shrink-0 rounded-md border border-[var(--color-line)] px-2 py-0.5 text-[12px]"
            :disabled="working.has(row.path)"
            @click="free(row.path)"
          >
            Free up space
          </button>
          <span class="w-16 shrink-0 text-right text-[12px] text-[var(--color-muted)]">
            {{ formatSize(row.size) }}
          </span>
        </li>
      </ul>
    </template>
  </div>
</template>
