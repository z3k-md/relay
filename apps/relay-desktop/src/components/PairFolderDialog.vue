<script setup lang="ts">
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { computed, onMounted, ref, watch } from "vue";
import Modal from "./Modal.vue";
import { api } from "../lib/api";
import { describeRemoteError, useRemoteFolders } from "../lib/remoteFolders";
import type { FolderPairParams, FolderPairPlan, PeerView } from "../lib/types";

/** Sync a folder on a paired device with a folder on this or another device. */
const props = defineProps<{
  sourceDevice: string;
  sourcePath: string;
  sourceName: string;
  /** Devices this computer may manage. */
  peers: PeerView[];
}>();

const emit = defineEmits<{
  close: [];
  paired: [];
}>();

/** "" is this computer. */
const destDevice = ref("");
const destParent = ref<string | null>(null);
const createNew = ref(true);
const newName = ref(props.sourceName);
const onlineOnly = ref(false);
const subfolders = ref<string[]>([]);
const included = ref<string[]>([]);
const plan = ref<FolderPairPlan | null>(null);
const busy = ref(false);
const error = ref<string | null>(null);
const picker = useRemoteFolders();

const destinations = computed(() => props.peers.filter((p) => p.name !== props.sourceDevice));
const destLabel = computed(() => destDevice.value || "this computer");

const params = computed<FolderPairParams | null>(() => {
  if (!destParent.value) return null;
  if (createNew.value && !newName.value.trim()) return null;
  return {
    source: { device: props.sourceDevice, path: props.sourcePath },
    dest: { device: destDevice.value || null, path: destParent.value },
    create_dest: createNew.value ? newName.value.trim() : null,
    name: null,
    excludes: subfolders.value.filter((name) => !included.value.includes(name)),
    dest_online_only: onlineOnly.value,
  };
});

// Any change makes the last check stale.
watch(params, () => {
  plan.value = null;
});

watch(destDevice, async (name) => {
  destParent.value = null;
  if (name) await picker.choose(name);
});

async function pickLocal() {
  const chosen = await openDialog({
    directory: true,
    multiple: false,
    title: createNew.value ? "Choose where to create the folder" : "Choose the folder to sync into",
  });
  if (typeof chosen === "string") destParent.value = chosen;
}

async function check() {
  const p = params.value;
  if (!p) return;
  busy.value = true;
  error.value = null;
  try {
    plan.value = await api.folderPairPreview(p);
  } catch (err) {
    error.value = describeRemoteError(err, destLabel.value);
  } finally {
    busy.value = false;
  }
}

async function start() {
  const p = params.value;
  if (!p) return;
  busy.value = true;
  error.value = null;
  try {
    await api.folderPair(p);
    emit("paired");
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  } finally {
    busy.value = false;
  }
}

onMounted(async () => {
  try {
    const reply = await api.remoteCall(props.sourceDevice, { call: "list_dir", path: props.sourcePath });
    if (reply.reply === "listing") {
      subfolders.value = reply.listing.entries
        .filter((e) => e.kind === "directory" && !e.hidden)
        .map((e) => e.name);
      included.value = [...subfolders.value];
    }
  } catch (err) {
    error.value = describeRemoteError(err, props.sourceDevice);
  }
});
</script>

<template>
  <Modal :open="true" :title="`Sync ${sourceName}`" @close="emit('close')">
    <p class="mono mb-3 truncate text-[12px] text-[var(--color-muted)]">
      {{ sourceDevice }}: {{ sourcePath }}
    </p>

    <label class="mb-1 block text-[12px] text-[var(--color-muted)]" for="pair-dest">Sync with</label>
    <select
      id="pair-dest"
      v-model="destDevice"
      class="mb-3 w-full rounded-md border border-[var(--color-line)] bg-[var(--color-canvas)] px-2 py-1"
    >
      <option value="">This computer</option>
      <option v-for="peer in destinations" :key="peer.id" :value="peer.name">{{ peer.name }}</option>
    </select>

    <label class="mb-2 flex items-center gap-2 text-[13px]">
      <input v-model="createNew" type="checkbox" />
      Create a new folder named
      <input
        v-model="newName"
        :disabled="!createNew"
        class="min-w-0 flex-1 rounded-md border border-[var(--color-line)] bg-[var(--color-canvas)] px-2 py-0.5"
      />
    </label>
    <p class="mb-1 text-[12px] text-[var(--color-muted)]">
      {{ createNew ? "Inside" : "Into" }} this folder on {{ destLabel }}:
    </p>

    <div v-if="!destDevice" class="mb-3 flex items-center gap-2">
      <p class="mono min-w-0 flex-1 truncate text-[12px]">{{ destParent ?? "Not chosen" }}</p>
      <button
        type="button"
        class="shrink-0 rounded-md border border-[var(--color-line)] px-2 py-0.5 text-[12px]"
        @click="pickLocal"
      >
        Choose…
      </button>
    </div>
    <div v-else class="mb-3 rounded-md border border-[var(--color-line)]">
      <div class="flex items-center gap-2 border-b border-[var(--color-line)] px-2 py-1">
        <button
          type="button"
          class="text-[12px] disabled:opacity-50"
          :disabled="!picker.listing.value || picker.loading.value"
          @click="picker.up()"
        >
          Up
        </button>
        <p class="mono min-w-0 flex-1 truncate text-[12px] text-[var(--color-muted)]">
          {{ picker.listing.value?.path ?? "Start here" }}
        </p>
        <button
          v-if="picker.listing.value"
          type="button"
          class="shrink-0 rounded-md border border-[var(--color-line)] px-2 text-[12px]"
          @click="destParent = picker.listing.value.path"
        >
          Use this folder
        </button>
      </div>
      <ul class="max-h-40 overflow-auto text-[13px]">
        <template v-if="!picker.listing.value">
          <li v-for="root in picker.roots.value" :key="root.path">
            <button type="button" class="w-full px-2 py-1 text-left" @click="picker.open(root.path)">
              {{ root.name }}
            </button>
          </li>
        </template>
        <template v-else>
          <li
            v-for="entry in picker.listing.value.entries.filter((e) => e.kind === 'directory' && !e.hidden)"
            :key="entry.path"
          >
            <button type="button" class="w-full px-2 py-1 text-left" @click="picker.open(entry.path)">
              📁 {{ entry.name }}
            </button>
          </li>
        </template>
      </ul>
      <p v-if="picker.error.value" class="px-2 py-1 text-[12px] text-[var(--color-danger)]">
        {{ picker.error.value }}
      </p>
      <p v-if="destParent" class="mono border-t border-[var(--color-line)] px-2 py-1 text-[12px]">
        Chosen: {{ destParent }}
      </p>
    </div>

    <template v-if="subfolders.length">
      <p class="mb-1 text-[12px] text-[var(--color-muted)]">Subfolders to sync</p>
      <div class="mb-3 max-h-32 overflow-auto rounded-md border border-[var(--color-line)] px-2 py-1">
        <label v-for="name in subfolders" :key="name" class="flex items-center gap-2 text-[13px]">
          <input v-model="included" type="checkbox" :value="name" />
          {{ name }}
        </label>
      </div>
    </template>

    <label class="mb-3 flex items-center gap-2 text-[13px]">
      <input v-model="onlineOnly" type="checkbox" />
      Download files on {{ destLabel }} only when opened
    </label>

    <div v-if="plan" class="mb-3 space-y-1 text-[12px]">
      <p v-for="problem in plan.problems" :key="problem" class="text-[var(--color-danger)]">{{ problem }}</p>
      <p v-for="warning in plan.warnings" :key="warning" class="text-amber-700 dark:text-amber-300">
        {{ warning }}
      </p>
      <p v-if="!plan.problems.length" class="text-[var(--color-muted)]">
        Ready: the space will be named {{ plan.space }}.
      </p>
    </div>
    <p v-if="error" class="mb-3 text-[12px] text-[var(--color-danger)]">{{ error }}</p>

    <div class="flex justify-end gap-2">
      <button type="button" class="rounded-md px-2.5 py-1" @click="emit('close')">Cancel</button>
      <button
        v-if="!plan || plan.problems.length"
        type="button"
        class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)] disabled:opacity-50"
        :disabled="busy || !params"
        @click="check"
      >
        Check
      </button>
      <button
        v-else
        type="button"
        class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)] disabled:opacity-50"
        :disabled="busy"
        @click="start"
      >
        Start syncing
      </button>
    </div>
  </Modal>
</template>
