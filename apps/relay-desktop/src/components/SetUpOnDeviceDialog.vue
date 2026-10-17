<script setup lang="ts">
import { computed, ref, watch } from "vue";
import Modal from "./Modal.vue";
import RemoteFolderPicker from "./RemoteFolderPicker.vue";
import { describeRemoteError } from "../lib/remoteFolders";
import { previewOn, remoteHome, setUpOnDevice, suggestPath } from "../lib/remoteSetup";
import type { PathPreview } from "../lib/types";

/** Sync a folder that is already here to any folder on another device. */
const props = defineProps<{
  /** Null when closed. */
  peer: string | null;
  thisDevice: string;
  spaceId: string;
  space: string;
  mount: string;
  localPath: string | null;
}>();

const emit = defineEmits<{
  close: [];
  done: [];
}>();

const path = ref("");
const browsing = ref(false);
const onlineOnly = ref(false);
const preview = ref<PathPreview | null>(null);
const checking = ref(false);
const busy = ref(false);
const error = ref<string | null>(null);
let previewGen = 0;

watch(
  () => props.peer,
  async (peer) => {
    path.value = "";
    preview.value = null;
    error.value = null;
    browsing.value = false;
    onlineOnly.value = false;
    if (!peer) return;
    try {
      const home = await remoteHome(peer);
      if (home && props.peer === peer) path.value = suggestPath(props.localPath, home, props.mount);
    } catch (err) {
      error.value = describeRemoteError(err, peer);
    }
  },
  { immediate: true },
);

let timer: number | undefined;
watch(path, (next) => {
  window.clearTimeout(timer);
  preview.value = null;
  if (!next.trim() || !props.peer) return;
  timer = window.setTimeout(() => void check(next.trim()), 300);
});

async function check(target: string) {
  const peer = props.peer;
  if (!peer) return;
  const gen = ++previewGen;
  checking.value = true;
  try {
    const result = await previewOn(peer, target);
    if (gen === previewGen) preview.value = result;
  } catch (err) {
    if (gen === previewGen) error.value = describeRemoteError(err, peer);
  } finally {
    if (gen === previewGen) checking.value = false;
  }
}

const problem = computed(() => {
  const p = preview.value;
  if (!p) return null;
  if (p.overlaps) return `That folder is inside or around ${p.overlaps.mount}, which already syncs.`;
  if (p.exists && !p.is_dir) return "That is a file, not a folder.";
  if (p.exists && !p.writable) return "Relay can't write there.";
  return null;
});

const note = computed(() => {
  const p = preview.value;
  if (!p) return null;
  if (!p.exists) return "Relay will create this folder.";
  if (p.files > 0) {
    const n = `${p.files.toLocaleString()}${p.truncated ? "+" : ""}`;
    return `${n} files are already there. Matching files are kept once; differing ones are kept as conflict copies.`;
  }
  if (p.cloud_only) return "Some files there are cloud placeholders and will download.";
  return "The folder is empty.";
});

async function start() {
  const peer = props.peer;
  if (!peer || problem.value) return;
  busy.value = true;
  error.value = null;
  try {
    await setUpOnDevice({
      peer,
      thisDevice: props.thisDevice,
      spaceId: props.spaceId,
      space: props.space,
      mount: props.mount,
      path: path.value.trim(),
      onlineOnly: onlineOnly.value,
    });
    emit("done");
  } catch (err) {
    error.value = describeRemoteError(err, peer);
  } finally {
    busy.value = false;
  }
}
</script>

<template>
  <Modal :open="!!peer" :title="`Sync ${mount} with ${peer ?? ''}`" @close="emit('close')">
    <label class="block text-[12px] text-[var(--color-muted)]" for="setup-path">Folder on {{ peer }}</label>
    <div class="mt-1 flex gap-2">
      <input
        id="setup-path"
        v-model="path"
        class="mono min-w-0 flex-1 rounded-md border border-[var(--color-line)] bg-[var(--color-canvas)] px-2 py-1.5 text-[12px]"
        spellcheck="false"
        autocomplete="off"
      />
      <button
        type="button"
        class="shrink-0 rounded-md border border-[var(--color-line)] px-2.5 py-1"
        @click="browsing = !browsing"
      >
        {{ browsing ? "Hide" : "Browse…" }}
      </button>
    </div>
    <RemoteFolderPicker
      v-if="browsing && peer"
      class="mt-2"
      :peer="peer"
      @pick="(picked) => { path = picked; browsing = false; }"
    />
    <p v-if="checking" class="mt-2 text-[12px] text-[var(--color-muted)]">Checking…</p>
    <p v-else-if="problem" class="mt-2 text-[12px] text-[var(--color-danger)]">{{ problem }}</p>
    <p v-else-if="note" class="mt-2 text-[12px] text-[var(--color-muted)]">{{ note }}</p>
    <label class="mt-3 flex items-start gap-2">
      <input v-model="onlineOnly" type="checkbox" class="mt-1" />
      <span>
        Online only on {{ peer }}
        <span class="block text-[12px] text-[var(--color-muted)]">Files download there when opened.</span>
      </span>
    </label>
    <p v-if="error" class="mt-3 text-[var(--color-danger)]">{{ error }}</p>
    <div class="mt-4 flex justify-end gap-2">
      <button type="button" class="rounded-md px-2.5 py-1" @click="emit('close')">Cancel</button>
      <button
        type="button"
        class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)] disabled:opacity-60"
        :disabled="busy || checking || !path.trim() || !!problem"
        @click="start"
      >
        {{ busy ? "Setting up…" : "Start syncing" }}
      </button>
    </div>
  </Modal>
</template>
