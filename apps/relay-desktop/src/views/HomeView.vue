<script setup lang="ts">
import { open } from "@tauri-apps/plugin-dialog";
import { computed, ref } from "vue";
import ErrorBanner from "../components/ErrorBanner.vue";
import Modal from "../components/Modal.vue";
import StatusDot from "../components/StatusDot.vue";
import { api, errorText } from "../lib/api";
import {
  baseName,
  devicesLabel,
  folderStatus,
  folderTransfers,
  folders,
  depth,
  freeMountName,
  freeName,
  home,
  loadHome,
  peerLine,
  pendingHolds,
  sharedWithPeer,
  type Folder,
} from "../lib/home";
import type { DeleteHold, OfferView, PeerView, TransferLive, UpdateAvailable } from "../lib/types";

const props = defineProps<{
  transfers: TransferLive[];
  update: UpdateAvailable | null;
  mobile: boolean;
}>();

const emit = defineEmits<{
  folder: [space: string, mount: string];
  device: [peer: string];
  addDevice: [];
  conflicts: [];
  installUpdate: [];
}>();

const busy = ref(false);
const error = ref<string | null>(null);
const confirm = ref<{ hold: DeleteHold; decision: "apply" | "restore" } | null>(null);

const list = computed(() => folders());

/** Conflicts per folder, for one Needs you row each. */
const conflictGroups = computed(() => {
  const groups = new Map<string, { label: string; count: number }>();
  for (const c of home.conflicts) {
    const key = `${c.space}/${c.mount}`;
    const group = groups.get(key) ?? { label: c.mount, count: 0 };
    group.count += 1;
    groups.set(key, group);
  }
  return [...groups.values()];
});

const needsYou = computed(
  () =>
    pendingHolds().length +
    conflictGroups.value.length +
    home.offers.length +
    (props.update && !props.mobile ? 1 : 0),
);

async function run(action: () => Promise<unknown>) {
  if (busy.value) return;
  busy.value = true;
  error.value = null;
  try {
    await action();
    await loadHome();
  } catch (err) {
    error.value = errorText(err);
  } finally {
    busy.value = false;
  }
}

async function chooseFolder(title: string): Promise<string | null> {
  const selected = await open({ directory: true, multiple: false, title });
  return typeof selected === "string" ? selected : null;
}

/**
 * A folder is a space with one mount named like it (folders-and-devices
 * proposal), except one around a folder already synced here, which joins
 * that folder's space so the inner folder keeps syncing as it is (D54).
 */
async function addFolder() {
  const path = await chooseFolder("Choose a folder to sync");
  if (!path) return;
  let parentOf: string | null = null;
  try {
    parentOf = await api.spaceInside(path);
  } catch (err) {
    error.value = errorText(err);
    return;
  }
  if (parentOf) {
    const space = parentOf;
    const mount = freeMountName(
      home.spaces.find((s) => s.name === space),
      baseName(path),
    );
    await run(() => api.addMount(space, mount, path));
    if (!error.value) emit("folder", space, mount);
    return;
  }
  const name = freeName(baseName(path));
  await run(async () => {
    await api.createSpace(name);
    try {
      await api.addMount(name, name, path);
    } catch (err) {
      await api.deleteSpace(name).catch(() => {});
      throw err;
    }
  });
  if (!error.value) emit("folder", name, name);
}

async function acceptOffer(offer: OfferView) {
  const path = await chooseFolder(`Where should ${offer.name} go on this device?`);
  if (!path) return;
  await run(async () => {
    await api.joinSpace(offer.name, offer.peer);
    const first = offer.mounts[0];
    if (first) await api.addMount(offer.name, first.name, path);
  });
}

function decide() {
  const pending = confirm.value;
  if (!pending) return;
  return run(async () => {
    await api.decideDeleteHold(
      pending.hold.space,
      pending.decision,
      pending.hold.mount,
      pending.hold.peerName,
    );
    confirm.value = null;
  });
}

function progress(folder: Folder): number | null {
  const rows = folderTransfers(folder, props.transfers).filter(
    (t) => t.direction === "receive" && t.bytesTotal,
  );
  if (!rows.length) return null;
  const done = rows.reduce((n, t) => n + t.bytesDone, 0);
  const total = rows.reduce((n, t) => n + (t.bytesTotal ?? 0), 0);
  return total > 0 ? Math.min(100, (done / total) * 100) : null;
}

function deviceFolders(peer: PeerView): string {
  const names = sharedWithPeer(peer).map((f) => f.mount.name);
  return names.length ? names.join(", ") : "No folders yet";
}
</script>

<template>
  <div class="mx-auto flex w-full max-w-3xl flex-col gap-6">
    <ErrorBanner :message="error ?? home.error" />

    <section
      v-if="needsYou"
      class="overflow-hidden rounded-lg border border-amber-300/70 bg-amber-50 dark:border-amber-900 dark:bg-amber-950/40"
    >
      <h2 class="px-3 pt-2.5 text-[11px] font-semibold uppercase tracking-wider text-[var(--color-warn)]">
        Needs you
      </h2>
      <ul class="divide-y divide-amber-200/70 dark:divide-amber-900/60">
        <li
          v-for="hold in pendingHolds()"
          :key="`${hold.peer}:${hold.space}:${hold.mount}`"
          class="flex flex-wrap items-center gap-x-3 gap-y-2 px-3 py-2.5"
        >
          <div class="min-w-0 flex-1">
            <p class="font-medium">
              {{ hold.peerName }} deleted {{ hold.deletions.toLocaleString() }} of
              {{ hold.live.toLocaleString() }} files in {{ hold.mount }}
            </p>
            <p class="text-[12px] text-[var(--color-muted)]">Nothing has been deleted here yet.</p>
          </div>
          <div class="flex gap-2">
            <button
              type="button"
              class="rounded-md border border-[var(--color-line)] bg-[var(--color-panel)] px-2.5 py-1"
              @click="confirm = { hold, decision: 'apply' }"
            >
              Delete here too
            </button>
            <button
              type="button"
              class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)]"
              @click="confirm = { hold, decision: 'restore' }"
            >
              Restore
            </button>
          </div>
        </li>
        <li
          v-for="offer in home.offers"
          :key="`${offer.peerId}:${offer.name}`"
          class="flex flex-wrap items-center gap-x-3 gap-y-2 px-3 py-2.5"
        >
          <div class="min-w-0 flex-1">
            <p class="font-medium">{{ offer.peer }} wants to sync {{ offer.name }} with this device</p>
            <p class="text-[12px] text-[var(--color-muted)]">Pick any folder here for it.</p>
          </div>
          <button
            type="button"
            class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)] disabled:opacity-60"
            :disabled="busy"
            @click="acceptOffer(offer)"
          >
            Choose a folder…
          </button>
        </li>
        <li
          v-for="group in conflictGroups"
          :key="group.label"
          class="flex flex-wrap items-center gap-x-3 gap-y-2 px-3 py-2.5"
        >
          <div class="min-w-0 flex-1">
            <p class="font-medium">
              {{ group.count === 1 ? "1 file" : `${group.count} files` }} in {{ group.label }}
              changed on two devices
            </p>
            <p class="text-[12px] text-[var(--color-muted)]">Both versions are kept until you pick one.</p>
          </div>
          <button
            type="button"
            class="rounded-md border border-[var(--color-line)] bg-[var(--color-panel)] px-2.5 py-1"
            @click="emit('conflicts')"
          >
            Review
          </button>
        </li>
        <li v-if="update && !mobile" class="flex flex-wrap items-center gap-x-3 gap-y-2 px-3 py-2.5">
          <div class="min-w-0 flex-1">
            <p class="font-medium">Relay {{ update.version }} is available</p>
            <p v-if="update.notes" class="line-clamp-2 text-[12px] text-[var(--color-muted)]">{{ update.notes }}</p>
          </div>
          <button
            type="button"
            class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)]"
            @click="emit('installUpdate')"
          >
            Install and restart
          </button>
        </li>
      </ul>
    </section>

    <section class="flex flex-col gap-2">
      <div class="flex items-center justify-between">
        <h2 class="text-[11px] font-semibold uppercase tracking-wider text-[var(--color-muted)]">Folders</h2>
        <button
          type="button"
          class="rounded-md px-2 py-0.5 text-[var(--color-accent)] hover:bg-[var(--color-panel)] disabled:opacity-60"
          :disabled="busy"
          @click="addFolder"
        >
          + Add folder
        </button>
      </div>
      <div
        v-if="home.loaded && !list.length"
        class="rounded-lg border border-dashed border-[var(--color-line)] px-4 py-6 text-center"
      >
        <p class="font-medium">No folders yet</p>
        <p class="mt-1 text-[var(--color-muted)]">
          Add a folder, then choose which of your devices keep a copy.
        </p>
        <button
          type="button"
          class="mt-3 rounded-md bg-[var(--color-accent)] px-3 py-1.5 text-[var(--color-accent-fg)]"
          @click="addFolder"
        >
          Add folder
        </button>
      </div>
      <ul
        v-else-if="list.length"
        class="divide-y divide-[var(--color-line)] overflow-hidden rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)]"
      >
        <li v-for="folder in list" :key="folder.key">
          <button
            type="button"
            class="flex w-full items-center gap-3 px-3 py-2.5 text-left hover:bg-[var(--color-canvas)]"
            :style="depth(folder) ? { paddingLeft: `${0.75 + 1.5 * depth(folder)}rem` } : undefined"
            @click="emit('folder', folder.space.name, folder.mount.name)"
          >
            <span
              class="grid size-8 shrink-0 place-items-center rounded-md bg-teal-50 text-[13px] font-semibold text-[var(--color-accent)] dark:bg-teal-950/60"
              aria-hidden="true"
            >
              {{ folder.mount.name.slice(0, 1).toUpperCase() }}
            </span>
            <span class="flex min-w-0 flex-1 flex-col">
              <span class="truncate font-medium">{{ folder.mount.name }}</span>
              <span class="truncate text-[12px] text-[var(--color-muted)]">
                {{ folder.inside ? `Inside ${folder.inside}` : devicesLabel(folder.space.sharedWith) }}
              </span>
              <span
                v-if="progress(folder) != null"
                class="mt-1 h-1 overflow-hidden rounded-full bg-[var(--color-line)]"
                role="progressbar"
                :aria-valuenow="Math.round(progress(folder) ?? 0)"
                aria-valuemin="0"
                aria-valuemax="100"
              >
                <span class="block h-full bg-[var(--color-accent)]" :style="{ width: `${progress(folder)}%` }" />
              </span>
            </span>
            <span class="flex shrink-0 items-center gap-2 text-[12px] text-[var(--color-muted)]">
              <span class="hidden max-w-56 truncate min-[520px]:inline">{{ folderStatus(folder, transfers).text }}</span>
              <StatusDot :tone="folderStatus(folder, transfers).tone" />
            </span>
          </button>
        </li>
      </ul>
    </section>

    <section class="flex flex-col gap-2">
      <div class="flex items-center justify-between">
        <h2 class="text-[11px] font-semibold uppercase tracking-wider text-[var(--color-muted)]">Devices</h2>
        <button
          type="button"
          class="rounded-md px-2 py-0.5 text-[var(--color-accent)] hover:bg-[var(--color-panel)]"
          @click="emit('addDevice')"
        >
          + Add device
        </button>
      </div>
      <div
        v-if="home.loaded && !home.peers.length"
        class="rounded-lg border border-dashed border-[var(--color-line)] px-4 py-6 text-center"
      >
        <p class="font-medium">No other devices yet</p>
        <p class="mt-1 text-[var(--color-muted)]">Pair your phone or another computer with a code or QR.</p>
        <button
          type="button"
          class="mt-3 rounded-md bg-[var(--color-accent)] px-3 py-1.5 text-[var(--color-accent-fg)]"
          @click="emit('addDevice')"
        >
          Add device
        </button>
      </div>
      <ul v-else class="grid gap-2 min-[520px]:grid-cols-2">
        <li v-for="peer in home.peers" :key="peer.id">
          <button
            type="button"
            class="flex w-full items-center gap-3 rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)] px-3 py-2.5 text-left hover:border-[var(--color-muted)]"
            @click="emit('device', peer.name)"
          >
            <StatusDot :tone="peer.connected ? 'ok' : 'off'" />
            <span class="flex min-w-0 flex-1 flex-col">
              <span class="truncate font-medium">{{ peer.name }}</span>
              <span class="truncate text-[12px] text-[var(--color-muted)]">
                {{ peerLine(peer) }} · {{ deviceFolders(peer) }}
              </span>
            </span>
            <span class="text-[var(--color-muted)]" aria-hidden="true">›</span>
          </button>
        </li>
      </ul>
    </section>

    <Modal
      :open="!!confirm"
      :title="confirm?.decision === 'apply' ? 'Delete these files here too?' : 'Restore these files?'"
      @close="confirm = null"
    >
      <p v-if="confirm?.decision === 'apply'">
        Delete {{ confirm.hold.deletions.toLocaleString() }} of {{ confirm.hold.live.toLocaleString() }}
        files in {{ confirm.hold.mount }} on this device too?
      </p>
      <p v-else-if="confirm">
        Keep the files here, including any already deleted during this catch-up, and send them
        back to {{ confirm.hold.peerName }}?
      </p>
      <div class="mt-4 flex justify-end gap-2">
        <button type="button" class="rounded-md px-2.5 py-1" @click="confirm = null">Cancel</button>
        <button
          type="button"
          class="rounded-md px-2.5 py-1 disabled:opacity-60"
          :class="confirm?.decision === 'apply' ? 'bg-red-600 text-white' : 'bg-[var(--color-accent)] text-[var(--color-accent-fg)]'"
          :disabled="busy"
          @click="decide"
        >
          {{ confirm?.decision === "apply" ? "Delete here too" : "Restore files" }}
        </button>
      </div>
    </Modal>
  </div>
</template>
