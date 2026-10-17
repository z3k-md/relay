<script setup lang="ts">
import { open } from "@tauri-apps/plugin-dialog";
import { openPath } from "@tauri-apps/plugin-opener";
import { computed, ref, watch } from "vue";
import ErrorBanner from "../components/ErrorBanner.vue";
import Modal from "../components/Modal.vue";
import SetUpOnDeviceDialog from "../components/SetUpOnDeviceDialog.vue";
import Sheet from "../components/Sheet.vue";
import StatusDot from "../components/StatusDot.vue";
import FilesView from "./FilesView.vue";
import { api, errorText } from "../lib/api";
import { findFolder, folderStatus, home, loadHome, nestedIn, peerLine, siblings } from "../lib/home";
import type { MaterializationMode, PeerView, TransferLive } from "../lib/types";

const props = defineProps<{
  space: string;
  mount: string;
  thisDevice: string;
  transfers: TransferLive[];
}>();

const emit = defineEmits<{
  close: [];
}>();

const folder = computed(() => findFolder(props.space, props.mount));
const busy = ref(false);
const error = ref<string | null>(null);
const notice = ref<string | null>(null);
/** What this device keeps for the whole folder. */
const mode = ref<MaterializationMode | null>(null);
const settingUp = ref<string | null>(null);
const confirm = ref<{ title: string; body: string; action: string; run: () => Promise<unknown> } | null>(
  null,
);

async function loadMode() {
  const f = folder.value;
  if (!f?.mount.attached) {
    mode.value = null;
    return;
  }
  try {
    mode.value = (await api.listFolder(props.space, props.mount, "")).mode;
  } catch {
    mode.value = null;
  }
}

watch(() => [props.space, props.mount, folder.value?.mount.attached], loadMode, { immediate: true });

async function run(action: () => Promise<unknown>) {
  if (busy.value) return;
  busy.value = true;
  error.value = null;
  notice.value = null;
  try {
    await action();
    await loadHome();
  } catch (err) {
    error.value = errorText(err);
  } finally {
    busy.value = false;
  }
}

function freeUp() {
  return run(async () => {
    const freed = await api.freeUpSpace(props.space, props.mount, "");
    notice.value = freed === 1 ? "Freed 1 file." : `Freed ${freed.toLocaleString()} files.`;
  });
}

function setMode(next: "full" | "demand") {
  return run(async () => {
    await api.setFolderMode(props.space, props.mount, "", next);
    await loadMode();
  });
}

const shared = computed(() => new Set(folder.value?.space.sharedWith ?? []));

function addDevice(peer: PeerView) {
  // A device that lets this one manage it is set up from here, in any
  // folder there. Any other device gets an offer and picks a folder itself.
  if (peer.connected && peer.canManage) {
    settingUp.value = peer.name;
    return;
  }
  return run(async () => {
    await api.share(props.space, peer.name);
    notice.value = peer.connected
      ? `${peer.name} will ask where to put ${props.mount}.`
      : `${peer.name} will ask where to put ${props.mount} the next time it is online.`;
  });
}

function removeDevice(peer: PeerView) {
  confirm.value = {
    title: `Stop syncing ${props.mount} with ${peer.name}?`,
    body: `Changes stop flowing between this device and ${peer.name}. Files stay on both.`,
    action: "Stop syncing",
    run: () => api.unshare(props.space, peer.name),
  };
}

function stopHere() {
  confirm.value = {
    title: `Stop syncing ${props.mount} on this device?`,
    body: `The files stay in ${folder.value?.mount.path ?? "the folder"}, and your other devices keep syncing it. You can pick a folder for it again later.`,
    action: "Stop syncing here",
    run: () => api.removeMount(props.space, props.mount),
  };
}

function forget() {
  confirm.value = {
    title: `Remove ${props.mount} from this device?`,
    body: "This device forgets the folder and stops sharing it. Other devices keep it and may offer it again.",
    action: "Remove",
    run: async () => {
      await api.deleteSpace(props.space);
      emit("close");
    },
  };
}

function runConfirmed() {
  const pending = confirm.value;
  if (!pending) return;
  return run(async () => {
    await pending.run();
    confirm.value = null;
  });
}

async function attach() {
  const selected = await open({ directory: true, multiple: false, title: `Where should ${props.mount} go?` });
  if (typeof selected === "string") await run(() => api.addMount(props.space, props.mount, selected));
}

async function openFolder() {
  const path = folder.value?.mount.path;
  if (!path) return;
  try {
    await openPath(path);
  } catch (err) {
    error.value = errorText(err);
  }
}
</script>

<template>
  <Sheet
    :title="mount"
    :subtitle="folder?.mount.path ?? 'Not on this device'"
    wide
    @close="emit('close')"
  >
    <template #actions>
      <button
        v-if="folder?.mount.path"
        type="button"
        class="rounded-md border border-[var(--color-line)] px-2.5 py-1"
        @click="openFolder"
      >
        Open folder
      </button>
    </template>

    <div v-if="folder" class="flex flex-col gap-5">
      <ErrorBanner :message="error" />
      <div class="flex items-center gap-2 text-[13px]">
        <StatusDot :tone="folderStatus(folder, transfers).tone" />
        <span>{{ folderStatus(folder, transfers).text }}</span>
      </div>
      <p v-if="notice" class="rounded-md bg-[var(--color-canvas)] px-3 py-2 text-[var(--color-muted)]">{{ notice }}</p>

      <div
        v-if="!folder.mount.attached"
        class="flex flex-wrap items-center justify-between gap-2 rounded-lg border border-[var(--color-line)] px-3 py-2.5"
      >
        <p>This device doesn't have a copy. Pick any folder for it.</p>
        <div class="flex gap-2">
          <button type="button" class="rounded-md px-2.5 py-1 text-[var(--color-muted)]" @click="forget">
            Remove
          </button>
          <button
            type="button"
            class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)]"
            :disabled="busy"
            @click="attach"
          >
            Choose a folder…
          </button>
        </div>
      </div>

      <section class="flex flex-col gap-2">
        <h3 class="text-[11px] font-semibold uppercase tracking-wider text-[var(--color-muted)]">Sync with</h3>
        <p v-if="!home.peers.length" class="text-[var(--color-muted)]">
          Add a device from the home screen first.
        </p>
        <ul
          v-else
          class="divide-y divide-[var(--color-line)] overflow-hidden rounded-lg border border-[var(--color-line)]"
        >
          <li v-for="peer in home.peers" :key="peer.id" class="flex items-center gap-3 px-3 py-2">
            <StatusDot :tone="peer.connected ? 'ok' : 'off'" />
            <span class="flex min-w-0 flex-1 flex-col">
              <span class="truncate font-medium">{{ peer.name }}</span>
              <span class="text-[12px] text-[var(--color-muted)]">{{ peerLine(peer) }}</span>
            </span>
            <button
              v-if="shared.has(peer.name)"
              type="button"
              class="rounded-md border border-[var(--color-line)] px-2.5 py-1 text-[12px]"
              :disabled="busy"
              @click="removeDevice(peer)"
            >
              Syncing · Stop
            </button>
            <button
              v-else
              type="button"
              class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[12px] text-[var(--color-accent-fg)] disabled:opacity-60"
              :disabled="busy"
              @click="addDevice(peer)"
            >
              Sync
            </button>
          </li>
        </ul>
        <p v-if="folder.inside" class="text-[12px] text-[var(--color-muted)]">
          Inside {{ folder.inside }} here, so it syncs with the same devices.
        </p>
        <p v-else-if="siblings(folder).length" class="text-[12px] text-[var(--color-muted)]">
          {{ siblings(folder).join(", ") }} {{ siblings(folder).length === 1 ? "shares" : "share" }}
          this list: devices added here sync those folders too.
        </p>
        <p v-if="nestedIn(folder).length" class="text-[12px] text-[var(--color-muted)]">
          {{ nestedIn(folder).join(", ") }} inside it {{ nestedIn(folder).length === 1 ? "syncs" : "sync" }}
          as {{ nestedIn(folder).length === 1 ? "its own folder" : "their own folders" }}, listed under it on
          the home screen.
        </p>
      </section>

      <section v-if="folder.mount.attached" class="flex flex-wrap items-center justify-between gap-2">
        <div>
          <h3 class="text-[11px] font-semibold uppercase tracking-wider text-[var(--color-muted)]">
            On this device
          </h3>
          <p class="text-[12px] text-[var(--color-muted)]">
            {{
              mode === "demand"
                ? "Files download when you open them."
                : "Every file is kept here."
            }}
          </p>
        </div>
        <div class="flex items-center gap-2">
          <button
            v-if="mode === 'demand'"
            type="button"
            class="rounded-md px-2 py-1 text-[var(--color-muted)] hover:text-[var(--color-ink)]"
            :disabled="busy"
            @click="freeUp"
          >
            Free up space
          </button>
          <div class="inline-flex overflow-hidden rounded-md border border-[var(--color-line)]" role="group">
            <button
              type="button"
              class="px-3 py-1"
              :class="mode !== 'demand' ? 'bg-[var(--color-accent)] text-[var(--color-accent-fg)]' : ''"
              :aria-pressed="mode !== 'demand'"
              :disabled="busy"
              @click="setMode('full')"
            >
              Keep all
            </button>
            <button
              type="button"
              class="border-l border-[var(--color-line)] px-3 py-1"
              :class="mode === 'demand' ? 'bg-[var(--color-accent)] text-[var(--color-accent-fg)]' : ''"
              :aria-pressed="mode === 'demand'"
              :disabled="busy"
              @click="setMode('demand')"
            >
              Online only
            </button>
          </div>
        </div>
      </section>

      <section v-if="folder.mount.attached" class="flex flex-col gap-2">
        <h3 class="text-[11px] font-semibold uppercase tracking-wider text-[var(--color-muted)]">Files</h3>
        <KeepAlive>
          <FilesView :key="folder.key" :only="{ space, mount }" :transfers="transfers" />
        </KeepAlive>
      </section>

      <section v-if="folder.mount.attached" class="border-t border-[var(--color-line)] pt-4">
        <button
          type="button"
          class="rounded-md border border-[var(--color-line)] px-2.5 py-1 text-[var(--color-danger)]"
          :disabled="busy"
          @click="stopHere"
        >
          Stop syncing on this device
        </button>
      </section>
    </div>
    <p v-else class="text-[var(--color-muted)]">This folder is gone.</p>

    <SetUpOnDeviceDialog
      v-if="folder"
      :peer="settingUp"
      :this-device="thisDevice"
      :space-id="folder.space.id"
      :space="space"
      :mount="mount"
      :local-path="folder.mount.path"
      @close="settingUp = null"
      @done="settingUp = null; loadHome()"
    />

    <Modal :open="!!confirm" :title="confirm?.title ?? ''" @close="confirm = null">
      <p>{{ confirm?.body }}</p>
      <div class="mt-4 flex justify-end gap-2">
        <button type="button" class="rounded-md px-2.5 py-1" @click="confirm = null">Cancel</button>
        <button
          type="button"
          class="rounded-md bg-red-600 px-2.5 py-1 text-white disabled:opacity-60"
          :disabled="busy"
          @click="runConfirmed"
        >
          {{ confirm?.action }}
        </button>
      </div>
    </Modal>
  </Sheet>
</template>
