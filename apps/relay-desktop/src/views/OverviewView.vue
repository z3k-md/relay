<script setup lang="ts">
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { computed, onMounted, onUnmounted, ref } from "vue";
import Modal from "../components/Modal.vue";
import StatusBadge from "../components/StatusBadge.vue";
import { api, copyText } from "../lib/api";
import { formatBytes } from "../lib/updateProgress";
import type { DeleteHold, Overview, RunnerState, TransferLive } from "../lib/types";

const props = defineProps<{
  overview: Overview;
  transfers: TransferLive[];
}>();

const emit = defineEmits<{
  pause: [];
  resume: [];
}>();

const copied = ref(false);
const holds = ref<DeleteHold[]>([]);
const confirm = ref<{ hold: DeleteHold; decision: "apply" | "restore" } | null>(null);
const busy = ref(false);
const holdError = ref<string | null>(null);
const unlistens: UnlistenFn[] = [];

async function loadHolds() {
  try {
    holds.value = await api.listDeleteHolds();
    holdError.value = null;
  } catch (err) {
    holdError.value = err instanceof Error ? err.message : String(err);
  }
}

async function decide() {
  const pending = confirm.value;
  if (!pending) return;
  busy.value = true;
  try {
    await api.decideDeleteHold(
      pending.hold.space,
      pending.decision,
      pending.hold.mount,
      pending.hold.peerName,
    );
    confirm.value = null;
    await loadHolds();
  } catch (err) {
    holdError.value = err instanceof Error ? err.message : String(err);
  } finally {
    busy.value = false;
  }
}

onMounted(async () => {
  await loadHolds();
  unlistens.push(
    await listen("relay://activity", (event) => {
      const kind = (event.payload as { kind?: string }).kind;
      if (kind === "deletesHeld" || kind === "reloading" || kind === "sync") {
        void loadHolds();
      }
    }),
  );
});

onUnmounted(() => {
  for (const off of unlistens) {
    off();
  }
});

const canToggle = computed(() => {
  const kind = props.overview.runner.kind;
  return kind === "running" || kind === "starting" || kind === "paused" || kind === "error";
});

async function copyId() {
  if (!props.overview.deviceId) return;
  await copyText(props.overview.deviceId);
  copied.value = true;
  window.setTimeout(() => {
    copied.value = false;
  }, 1500);
}

function transferTitle(row: TransferLive): string {
  if (row.direction === "index") {
    return `Indexing ${row.mount ?? row.space}`;
  }
  const verb = row.direction === "receive" ? "Receiving from" : "Sending to";
  return `${verb} ${row.peerName} · ${row.space}`;
}

function transferFiles(row: TransferLive): string {
  if (row.filesTotal != null) return `${row.filesDone} of ${row.filesTotal} files`;
  return `${row.filesDone} files`;
}

function transferBytes(row: TransferLive): string {
  if (row.direction === "receive" && row.bytesTotal != null) {
    return `${formatBytes(row.bytesDone)} of ${formatBytes(row.bytesTotal)}`;
  }
  if (row.direction === "index") return `${formatBytes(row.bytesDone)} hashed`;
  return formatBytes(row.bytesDone);
}

function transferPercent(row: TransferLive): number | null {
  if (row.direction !== "receive" || row.bytesTotal == null || row.bytesTotal <= 0) return null;
  return Math.max(0, Math.min(100, (row.bytesDone / row.bytesTotal) * 100));
}

function showRate(row: TransferLive): boolean {
  return props.overview.runner.kind !== "paused" && row.bytesPerSec > 0 && row.direction !== "index";
}

function runnerDetail(state: RunnerState): string | null {
  if (state.kind === "error" || state.kind === "externalService") {
    return state.message;
  }
  return null;
}
</script>

<template>
  <div>
    <div class="mb-4 flex flex-wrap items-start justify-between gap-3">
      <div>
        <h2 class="text-[18px] font-semibold tracking-tight">{{ overview.deviceName }}</h2>
        <p class="text-[var(--color-muted)]">This computer</p>
      </div>
      <div class="flex items-center gap-2">
        <StatusBadge :state="overview.runner" />
        <button
          v-if="canToggle && overview.runner.kind === 'paused'"
          type="button"
          class="rounded-md border border-[var(--color-line)] px-2.5 py-1"
          @click="emit('resume')"
        >
          Resume
        </button>
        <button
          v-else-if="canToggle"
          type="button"
          class="rounded-md border border-[var(--color-line)] px-2.5 py-1"
          @click="emit('pause')"
        >
          Pause
        </button>
      </div>
    </div>

    <p
      v-if="runnerDetail(overview.runner)"
      class="mb-4 rounded-md border border-[var(--color-line)] bg-[var(--color-panel)] px-3 py-2 text-[var(--color-muted)]"
    >
      {{ runnerDetail(overview.runner) }}
    </p>

    <div
      v-for="hold in holds.filter((h) => !h.decision)"
      :key="`${hold.peer}:${hold.space}:${hold.mount}`"
      class="mb-4 rounded-lg border border-amber-300/70 bg-amber-50 px-3 py-2 dark:border-amber-900 dark:bg-amber-950/50"
    >
      <p class="font-medium">
        {{ hold.peerName }} wants to delete {{ hold.deletions }} of {{ hold.live }} files in
        {{ hold.space }}/{{ hold.mount }}. Nothing has been deleted yet.
      </p>
      <div class="mt-2 flex flex-wrap gap-2">
        <button
          type="button"
          class="rounded-md border border-[var(--color-line)] px-2.5 py-1"
          @click="confirm = { hold, decision: 'apply' }"
        >
          Delete them here too
        </button>
        <button
          type="button"
          class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)]"
          @click="confirm = { hold, decision: 'restore' }"
        >
          Restore files here and on {{ hold.peerName }}
        </button>
      </div>
    </div>
    <p v-if="holdError" class="mb-4 text-[var(--color-muted)]">{{ holdError }}</p>

    <section
      v-if="transfers.length"
      class="mb-4 rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)] px-3 py-2"
    >
      <p class="mb-2 text-[12px] font-medium text-[var(--color-muted)]">
        {{ overview.runner.kind === "paused" ? "Sync paused" : "Syncing" }}
      </p>
      <div v-for="row in transfers" :key="`${row.direction}-${row.peerId}-${row.space}-${row.mount}`" class="mb-3 last:mb-0">
        <div class="flex items-baseline justify-between gap-3">
          <p class="font-medium">{{ transferTitle(row) }}</p>
          <p class="shrink-0 tabular-nums text-[12px] text-[var(--color-muted)]">
            {{ transferBytes(row) }}
            <template v-if="showRate(row)"> · {{ formatBytes(row.bytesPerSec) }}/s</template>
          </p>
        </div>
        <p class="text-[12px] text-[var(--color-muted)]">
          {{ transferFiles(row) }}
          <template v-if="row.currentPath"> · {{ row.currentPath }}</template>
          <template v-if="row.retries > 0"> · {{ row.retries }} files waiting to retry</template>
        </p>
        <div
          v-if="transferPercent(row) != null"
          class="mt-1.5 h-1.5 overflow-hidden rounded-full bg-[var(--color-line)]"
          role="progressbar"
          :aria-valuemin="0"
          :aria-valuemax="100"
          :aria-valuenow="Math.round(transferPercent(row) ?? 0)"
        >
          <div
            class="h-full rounded-full bg-[var(--color-accent)]"
            :style="{ width: `${transferPercent(row)}%` }"
          />
        </div>
      </div>
    </section>

    <Modal
      :open="!!confirm"
      :title="confirm?.decision === 'apply' ? 'Delete these files?' : 'Restore these files?'"
      @close="confirm = null"
    >
      <p v-if="confirm?.decision === 'apply'">
        Delete {{ confirm.hold.deletions }} of {{ confirm.hold.live }} files in
        {{ confirm.hold.space }}/{{ confirm.hold.mount }} on this computer too?
      </p>
      <p v-else-if="confirm">
        Keep the files here — including any already deleted this catch-up — and
        send them back to {{ confirm.hold.peerName }}?
      </p>
      <div class="mt-4 flex justify-end gap-2">
        <button type="button" class="rounded-md px-2.5 py-1" @click="confirm = null">Cancel</button>
        <button
          type="button"
          class="rounded-md px-2.5 py-1 text-white disabled:opacity-60"
          :class="confirm?.decision === 'apply' ? 'bg-red-600' : 'bg-[var(--color-accent)] text-[var(--color-accent-fg)]'"
          :disabled="busy"
          @click="decide"
        >
          {{ confirm?.decision === "apply" ? "Delete them here too" : "Restore files" }}
        </button>
      </div>
    </Modal>

    <div class="grid gap-3 sm:grid-cols-2">
      <section class="rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)] p-3">
        <p class="text-[12px] font-medium text-[var(--color-muted)]">Device id</p>
        <div class="mt-1 flex items-start gap-2">
          <code class="mono min-w-0 flex-1 break-all text-[12px]">{{ overview.deviceId }}</code>
          <button
            type="button"
            class="shrink-0 rounded-md border border-[var(--color-line)] px-2 py-1"
            @click="copyId"
          >
            {{ copied ? "Copied" : "Copy" }}
          </button>
        </div>
        <p class="mt-2 text-[12px] text-[var(--color-muted)]">
          Give this id to your other device when you add this computer as a peer.
        </p>
      </section>
      <section class="rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)] p-3">
        <p class="text-[12px] font-medium text-[var(--color-muted)]">At a glance</p>
        <dl class="mt-2 grid grid-cols-2 gap-2">
          <div>
            <dt class="text-[var(--color-muted)]">Peers online</dt>
            <dd class="text-lg font-semibold">{{ overview.connectedPeers }} / {{ overview.peerCount }}</dd>
          </div>
          <div>
            <dt class="text-[var(--color-muted)]">Spaces</dt>
            <dd class="text-lg font-semibold">{{ overview.spaceCount }}</dd>
          </div>
          <div>
            <dt class="text-[var(--color-muted)]">Folders</dt>
            <dd class="text-lg font-semibold">{{ overview.mountCount }}</dd>
          </div>
          <div>
            <dt class="text-[var(--color-muted)]">Version</dt>
            <dd class="mono text-[12px]">{{ overview.version }}</dd>
          </div>
        </dl>
      </section>
    </div>
  </div>
</template>
