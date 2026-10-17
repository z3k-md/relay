<script setup lang="ts">
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { computed, ref } from "vue";
import ErrorBanner from "../components/ErrorBanner.vue";
import Modal from "../components/Modal.vue";
import Sheet from "../components/Sheet.vue";
import SpeedTestDialog from "../components/SpeedTestDialog.vue";
import StatusDot from "../components/StatusDot.vue";
import { api, copyText, errorText } from "../lib/api";
import { home, loadHome, peerLine, sharedWithPeer } from "../lib/home";

const props = defineProps<{
  peer: string;
}>();

const emit = defineEmits<{
  close: [];
  browse: [peer: string];
  folder: [space: string, mount: string];
}>();

const device = computed(() => home.peers.find((p) => p.name === props.peer) ?? null);
const busy = ref(false);
const error = ref<string | null>(null);
const notice = ref<string | null>(null);
const testing = ref<string | null>(null);
const removing = ref(false);
const copied = ref(false);

const browseWhyNot = computed(() => {
  const d = device.value;
  if (!d) return null;
  if (!d.connected) return `${d.name} is offline.`;
  if (!d.canManage) return `${d.name} hasn't allowed this device to set up sync on it.`;
  return null;
});

async function sendFile() {
  const d = device.value;
  if (!d) return;
  const selected = await openDialog({ multiple: true, title: `Send to ${d.name}` });
  const paths = typeof selected === "string" ? [selected] : (selected ?? []);
  if (!paths.length) return;
  busy.value = true;
  error.value = null;
  notice.value = null;
  try {
    const names: string[] = [];
    for (const path of paths) names.push((await api.dropSend(d.name, path)).saved_name);
    notice.value = `Sent ${names.join(", ")} to ${d.name}.`;
  } catch (err) {
    error.value = errorText(err);
  } finally {
    busy.value = false;
  }
}

async function toggleManage() {
  const d = device.value;
  if (!d) return;
  busy.value = true;
  error.value = null;
  try {
    await api.setPeerManage(d.name, !d.allowedToManage);
    await loadHome();
  } catch (err) {
    error.value = errorText(err);
  } finally {
    busy.value = false;
  }
}

async function remove() {
  busy.value = true;
  error.value = null;
  try {
    await api.removePeer(props.peer);
    removing.value = false;
    await loadHome();
    emit("close");
  } catch (err) {
    error.value = errorText(err);
  } finally {
    busy.value = false;
  }
}

async function copyId() {
  if (!device.value) return;
  await copyText(device.value.id);
  copied.value = true;
  window.setTimeout(() => (copied.value = false), 1500);
}
</script>

<template>
  <Sheet :title="peer" @close="emit('close')">
    <div v-if="device" class="flex flex-col gap-5">
      <ErrorBanner :message="error" />
      <div class="flex items-center gap-2">
        <StatusDot :tone="device.connected ? 'ok' : 'off'" />
        <span>{{ peerLine(device) }}</span>
      </div>
      <p v-if="notice" class="rounded-md bg-[var(--color-canvas)] px-3 py-2 text-[var(--color-muted)]">{{ notice }}</p>

      <ul class="divide-y divide-[var(--color-line)] overflow-hidden rounded-lg border border-[var(--color-line)]">
        <li>
          <button
            type="button"
            class="flex w-full items-center justify-between px-3 py-2.5 text-left hover:bg-[var(--color-canvas)] disabled:opacity-50"
            :disabled="busy || !device.connected"
            @click="sendFile"
          >
            <span>
              <span class="block font-medium">Send a file</span>
              <span class="block text-[12px] text-[var(--color-muted)]">It pops up on {{ device.name }}, ready to use.</span>
            </span>
            <span aria-hidden="true">›</span>
          </button>
        </li>
        <li>
          <button
            type="button"
            class="flex w-full items-center justify-between px-3 py-2.5 text-left hover:bg-[var(--color-canvas)] disabled:opacity-50"
            :disabled="!!browseWhyNot"
            @click="emit('browse', device.name)"
          >
            <span>
              <span class="block font-medium">Browse files</span>
              <span class="block text-[12px] text-[var(--color-muted)]">
                {{ browseWhyNot ?? `Open a file from ${device.name} or sync one of its folders here.` }}
              </span>
            </span>
            <span aria-hidden="true">›</span>
          </button>
        </li>
        <li>
          <button
            type="button"
            class="flex w-full items-center justify-between px-3 py-2.5 text-left hover:bg-[var(--color-canvas)] disabled:opacity-50"
            :disabled="!device.connected"
            @click="testing = device.name"
          >
            <span>
              <span class="block font-medium">Test connection</span>
              <span class="block text-[12px] text-[var(--color-muted)]">Speed and latency between the two devices.</span>
            </span>
            <span aria-hidden="true">›</span>
          </button>
        </li>
      </ul>

      <section class="flex flex-col gap-2">
        <h3 class="text-[11px] font-semibold uppercase tracking-wider text-[var(--color-muted)]">Folders</h3>
        <p v-if="!sharedWithPeer(device).length" class="text-[var(--color-muted)]">
          No folders sync with {{ device.name }} yet. Open a folder and choose Sync.
        </p>
        <ul v-else class="divide-y divide-[var(--color-line)] overflow-hidden rounded-lg border border-[var(--color-line)]">
          <li v-for="f in sharedWithPeer(device)" :key="f.key">
            <button
              type="button"
              class="flex w-full items-center justify-between px-3 py-2 text-left hover:bg-[var(--color-canvas)]"
              @click="emit('folder', f.space.name, f.mount.name)"
            >
              <span class="font-medium">{{ f.mount.name }}</span>
              <span aria-hidden="true">›</span>
            </button>
          </li>
        </ul>
      </section>

      <label class="flex items-start justify-between gap-3 rounded-lg border border-[var(--color-line)] px-3 py-2.5">
        <span>
          <span class="block font-medium">Can set up sync here</span>
          <span class="block text-[12px] text-[var(--color-muted)]">
            {{ device.name }} can browse this device and sync its folders, as you can from here. Turn
            off for a device you don't fully control. Credential folders stay private either way.
          </span>
        </span>
        <input
          type="checkbox"
          class="mt-1"
          :checked="device.allowedToManage"
          :disabled="busy"
          @change="toggleManage"
        />
      </label>

      <details class="rounded-lg border border-[var(--color-line)] px-3 py-2">
        <summary class="cursor-pointer select-none font-medium">Details</summary>
        <dl class="mt-2 grid grid-cols-[auto_1fr] gap-x-3 gap-y-1.5 text-[12px]">
          <dt class="text-[var(--color-muted)]">Device id</dt>
          <dd class="flex items-start gap-2">
            <code class="mono min-w-0 flex-1 break-all">{{ device.id }}</code>
            <button type="button" class="shrink-0 rounded border border-[var(--color-line)] px-1.5" @click="copyId">
              {{ copied ? "Copied" : "Copy" }}
            </button>
          </dd>
          <dt class="text-[var(--color-muted)]">Address</dt>
          <dd class="mono">{{ device.address || "Found on the local network" }}</dd>
        </dl>
      </details>

      <div class="border-t border-[var(--color-line)] pt-4">
        <button
          type="button"
          class="rounded-md border border-[var(--color-line)] px-2.5 py-1 text-[var(--color-danger)]"
          @click="removing = true"
        >
          Remove device
        </button>
      </div>
    </div>
    <p v-else class="text-[var(--color-muted)]">This device was removed.</p>

    <SpeedTestDialog :peer="testing" @close="testing = null" />
    <Modal :open="removing" :title="`Remove ${peer}?`" @close="removing = false">
      <p>
        This device stops trusting {{ peer }} and stops syncing with it. Files stay on both. To add
        it back, pair again.
      </p>
      <div class="mt-4 flex justify-end gap-2">
        <button type="button" class="rounded-md px-2.5 py-1" @click="removing = false">Cancel</button>
        <button
          type="button"
          class="rounded-md bg-red-600 px-2.5 py-1 text-white disabled:opacity-60"
          :disabled="busy"
          @click="remove"
        >
          Remove
        </button>
      </div>
    </Modal>
  </Sheet>
</template>
