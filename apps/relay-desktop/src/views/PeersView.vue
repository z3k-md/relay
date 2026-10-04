<script setup lang="ts">
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { computed, onMounted, onUnmounted, ref } from "vue";
import EmptyState from "../components/EmptyState.vue";
import ErrorBanner from "../components/ErrorBanner.vue";
import Modal from "../components/Modal.vue";
import { api } from "../lib/api";
import type { ActivityItem, PeerView, SpaceView } from "../lib/types";

const peers = ref<PeerView[]>([]);
const spaces = ref<SpaceView[]>([]);
const loading = ref(true);
const error = ref<string | null>(null);
const adding = ref(false);
const pairing = ref(false);
const joining = ref(false);
const name = ref("");
const deviceId = ref("");
const address = ref("");
const confirmName = ref<string | null>(null);
const busy = ref(false);
const share = ref<string[]>([]);
const pairCode = ref("");
const pairExpires = ref(0);
const pairNow = ref(Date.now());
const pairDone = ref<string | null>(null);
const pairFailed = ref<string | null>(null);
const joinCode = ref("");
const joinAddr = ref("");
/** Pairing lets the other device manage this one unless unchecked (D37). */
const allowManage = ref(true);
const nowMs = ref(Date.now());
let statusTimer: number | undefined;
let tickTimer: number | undefined;
let clockTimer: number | undefined;
let stopActivity: UnlistenFn | undefined;
let loadGen = 0;

const remaining = computed(() => {
  const ms = pairExpires.value - pairNow.value;
  if (ms <= 0) return "expired";
  const total = Math.ceil(ms / 1000);
  const minutes = Math.floor(total / 60);
  const seconds = total % 60;
  return `${minutes}:${seconds.toString().padStart(2, "0")}`;
});

async function load(silent = false) {
  const gen = ++loadGen;
  if (!silent) {
    loading.value = true;
    error.value = null;
  }
  try {
    const [nextPeers, nextSpaces] = await Promise.all([api.listPeers(), api.listSpaces()]);
    if (gen !== loadGen) return;
    peers.value = nextPeers;
    spaces.value = nextSpaces;
    error.value = null;
  } catch (err) {
    if (gen !== loadGen) return;
    error.value = err instanceof Error ? err.message : String(err);
  } finally {
    if (gen === loadGen) loading.value = false;
  }
}

function formatSpan(ms: number): string {
  const sec = Math.max(0, Math.floor(ms / 1000));
  if (sec < 10) return "a few seconds";
  if (sec < 60) return `${sec} seconds`;
  const min = Math.floor(sec / 60);
  if (min < 60) return min === 1 ? "1 minute" : `${min} minutes`;
  const hours = Math.floor(min / 60);
  const remMin = min % 60;
  if (hours < 24) {
    const hourLabel = hours === 1 ? "1 hour" : `${hours} hours`;
    return remMin === 0 ? hourLabel : `${hours}h ${remMin}m`;
  }
  const days = Math.floor(hours / 24);
  const remH = hours % 24;
  const dayLabel = days === 1 ? "1 day" : `${days} days`;
  return remH === 0 ? dayLabel : `${days}d ${remH}h`;
}

function formatSeen(ms: number): string {
  const then = new Date(ms);
  const today = new Date(nowMs.value);
  const time = new Intl.DateTimeFormat(undefined, {
    hour: "numeric",
    minute: "2-digit",
  }).format(then);
  const startOf = (date: Date) => new Date(date.getFullYear(), date.getMonth(), date.getDate()).getTime();
  const dayDelta = Math.round((startOf(today) - startOf(then)) / 86_400_000);
  if (dayDelta === 0) return `today at ${time}`;
  if (dayDelta === 1) return `yesterday at ${time}`;
  const date = new Intl.DateTimeFormat(undefined, {
    month: "short",
    day: "numeric",
    year: then.getFullYear() === today.getFullYear() ? undefined : "numeric",
  }).format(then);
  return `${date} at ${time}`;
}

function peerStatus(peer: PeerView): string {
  if (peer.connected) {
    if (peer.connectedSinceMs != null) {
      return `Online for ${formatSpan(nowMs.value - peer.connectedSinceMs)}`;
    }
    return "Online";
  }
  if (peer.lastSeenMs != null) {
    return `Offline for ${formatSpan(nowMs.value - peer.lastSeenMs)}`;
  }
  return "Not seen yet";
}

function statusClass(peer: PeerView): string {
  if (peer.connected) return "text-emerald-700 dark:text-emerald-300";
  if (peer.lastSeenMs != null) return "";
  return "text-[var(--color-muted)]";
}

function sharedLabel(peer: PeerView): string {
  const names = spaces.value
    .filter((space) => space.sharedWith.includes(peer.name))
    .map((space) => space.name);
  return names.join(", ");
}

function stopPairWatch() {
  if (statusTimer !== undefined) {
    window.clearInterval(statusTimer);
    statusTimer = undefined;
  }
  if (tickTimer !== undefined) {
    window.clearInterval(tickTimer);
    tickTimer = undefined;
  }
}

async function startPair() {
  busy.value = true;
  error.value = null;
  pairDone.value = null;
  pairFailed.value = null;
  try {
    const started = await api.pairStart(share.value, allowManage.value);
    pairCode.value = started.code;
    pairExpires.value = started.expiresAtMs;
    pairNow.value = Date.now();
    pairing.value = true;
    tickTimer = window.setInterval(() => {
      pairNow.value = Date.now();
    }, 1000);
    statusTimer = window.setInterval(async () => {
      try {
        const status = await api.pairStatus();
        if (status.state === "paired") {
          pairDone.value = status.peerName;
          stopPairWatch();
          await load();
        } else if (status.state === "failed") {
          pairFailed.value = status.reason;
          stopPairWatch();
        } else if (status.state === "expired") {
          pairFailed.value = "This code expired. Start a new pairing.";
          stopPairWatch();
        }
      } catch (err) {
        pairFailed.value = err instanceof Error ? err.message : String(err);
        stopPairWatch();
      }
    }, 750);
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
    pairing.value = false;
  } finally {
    busy.value = false;
  }
}

async function closePair() {
  stopPairWatch();
  if (pairCode.value && !pairDone.value && !pairFailed.value) {
    try {
      await api.pairCancel();
    } catch {
      /* already closed or host gone */
    }
  }
  pairing.value = false;
  pairCode.value = "";
  pairFailed.value = null;
  pairDone.value = null;
}

async function joinPair() {
  busy.value = true;
  error.value = null;
  try {
    await api.pairJoin(
      joinCode.value.trim(),
      joinAddr.value.trim() || undefined,
      allowManage.value,
    );
    joining.value = false;
    joinCode.value = "";
    joinAddr.value = "";
    await load();
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  } finally {
    busy.value = false;
  }
}

async function addPeer() {
  busy.value = true;
  error.value = null;
  try {
    await api.addPeer(name.value.trim(), deviceId.value.trim(), address.value.trim());
    adding.value = false;
    name.value = "";
    deviceId.value = "";
    address.value = "";
    await load();
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  } finally {
    busy.value = false;
  }
}

async function toggleManage(peer: PeerView) {
  busy.value = true;
  error.value = null;
  try {
    await api.setPeerManage(peer.name, !peer.allowedToManage);
    await load(true);
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  } finally {
    busy.value = false;
  }
}

async function removePeer() {
  if (!confirmName.value) return;
  busy.value = true;
  error.value = null;
  try {
    await api.removePeer(confirmName.value);
    confirmName.value = null;
    await load();
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  } finally {
    busy.value = false;
  }
}

onMounted(async () => {
  nowMs.value = Date.now();
  clockTimer = window.setInterval(() => {
    nowMs.value = Date.now();
  }, 1000);
  stopActivity = await listen<ActivityItem>("relay://activity", (event) => {
    if (event.payload.kind === "peerConnected" || event.payload.kind === "peerDisconnected") {
      void load(true);
    }
  });
  await load();
});
onUnmounted(() => {
  stopPairWatch();
  if (clockTimer !== undefined) window.clearInterval(clockTimer);
  stopActivity?.();
  if (pairCode.value && !pairDone.value) {
    api.pairCancel().catch(() => {});
  }
});
defineExpose({ load });
</script>

<template>
  <div>
    <div class="mb-3 flex items-center justify-between gap-2">
      <h2 class="text-[15px] font-semibold">Peers</h2>
      <div class="flex flex-wrap justify-end gap-2">
        <button
          type="button"
          class="rounded-md border border-[var(--color-line)] px-2.5 py-1"
          @click="joining = true"
        >
          Enter a code
        </button>
        <button
          type="button"
          class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)]"
          @click="pairing = true"
        >
          Pair a device
        </button>
      </div>
    </div>
    <ErrorBanner :message="error" />
    <p v-if="loading" class="text-[var(--color-muted)]">Loading peers…</p>
    <EmptyState
      v-else-if="peers.length === 0"
      title="No peers yet"
      body="Pair another computer with a short code. On the same network that is the whole step. Over a VPN, add its address."
    >
      <div class="flex flex-wrap gap-2">
        <button
          type="button"
          class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)]"
          @click="pairing = true"
        >
          Pair a device
        </button>
        <button
          type="button"
          class="rounded-md border border-[var(--color-line)] px-2.5 py-1"
          @click="joining = true"
        >
          Enter a code
        </button>
      </div>
    </EmptyState>
    <ul v-else class="space-y-2">
      <li
        v-for="peer in peers"
        :key="peer.id"
        class="flex items-start justify-between gap-3 rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)] px-3 py-2"
      >
        <div class="min-w-0">
          <div class="flex items-center gap-2">
            <span
              class="inline-block h-2 w-2 shrink-0 rounded-full"
              :class="peer.connected ? 'bg-emerald-500' : 'bg-zinc-400'"
              :title="peerStatus(peer)"
            />
            <span class="font-medium">{{ peer.name }}</span>
            <span class="mono text-[12px] text-[var(--color-muted)]">{{ peer.shortId }}</span>
          </div>
          <p class="mt-0.5 text-[13px]" :class="statusClass(peer)">
            {{ peerStatus(peer) }}
          </p>
          <p
            v-if="!peer.connected && peer.lastSeenMs != null"
            class="text-[12px] text-[var(--color-muted)]"
          >
            Last seen {{ formatSeen(peer.lastSeenMs) }}
          </p>
          <p class="truncate text-[12px] text-[var(--color-muted)]">
            {{ peer.address || "No address" }}
          </p>
          <p v-if="sharedLabel(peer)" class="truncate text-[12px] text-[var(--color-muted)]">
            Spaces: {{ sharedLabel(peer) }}
          </p>
          <label class="mt-1 flex items-center gap-2 text-[12px]">
            <input
              type="checkbox"
              :checked="peer.allowedToManage"
              :disabled="busy"
              @change="toggleManage(peer)"
            />
            <span>Can browse and set up sync on this computer</span>
          </label>
          <p v-if="peer.canManage" class="text-[12px] text-[var(--color-muted)]">
            You can browse {{ peer.name }} from Browse.
          </p>
        </div>
        <button
          type="button"
          class="mt-0.5 shrink-0 text-[var(--color-danger)]"
          @click="confirmName = peer.name"
        >
          Remove
        </button>
      </li>
    </ul>
    <p class="mt-3 text-[12px] text-[var(--color-muted)]">
      <button type="button" class="underline" @click="adding = true">Add a peer by device id</button>
      if you already know the other machine’s id and address.
    </p>

    <Modal :open="pairing" title="Pair a device" @close="closePair">
      <div v-if="pairDone">
        <p>
          Paired with <strong>{{ pairDone }}</strong
          >. They can join any spaces you shared from their Spaces view.
        </p>
        <div class="mt-4 flex justify-end">
          <button
            type="button"
            class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)]"
            @click="closePair"
          >
            Done
          </button>
        </div>
      </div>
      <div v-else-if="pairFailed">
        <p class="text-[var(--color-danger)]">{{ pairFailed }}</p>
        <div class="mt-4 flex justify-end">
          <button type="button" class="rounded-md px-2.5 py-1" @click="closePair">Close</button>
        </div>
      </div>
      <div v-else-if="pairCode">
        <p class="text-[12px] text-[var(--color-muted)]">
          Enter this code on the other device. It expires in {{ remaining }}.
        </p>
        <p class="mono my-4 text-center text-[28px] font-semibold tracking-wide">{{ pairCode }}</p>
        <p class="text-[12px] text-[var(--color-muted)]">Waiting for the other device…</p>
        <div class="mt-4 flex justify-end">
          <button type="button" class="rounded-md px-2.5 py-1" @click="closePair">Cancel</button>
        </div>
      </div>
      <div v-else>
        <p class="mb-3 text-[12px] text-[var(--color-muted)]">
          Optionally share spaces now. The other device still has to join them.
        </p>
        <label
          v-for="space in spaces"
          :key="space.id"
          class="mb-2 flex items-center gap-2"
        >
          <input v-model="share" type="checkbox" :value="space.name" />
          <span>{{ space.name }}</span>
        </label>
        <p v-if="spaces.length === 0" class="text-[12px] text-[var(--color-muted)]">
          No spaces yet — you can share later.
        </p>
        <label class="mt-3 flex items-start gap-2">
          <input v-model="allowManage" type="checkbox" class="mt-1" />
          <span>
            Let the other device browse this computer and set up sync on it
            <span class="block text-[12px] text-[var(--color-muted)]">
              You can change this later in Peers.
            </span>
          </span>
        </label>
        <div class="mt-4 flex justify-end gap-2">
          <button type="button" class="rounded-md px-2.5 py-1" @click="closePair">Cancel</button>
          <button
            type="button"
            class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)] disabled:opacity-60"
            :disabled="busy"
            @click="startPair"
          >
            Show code
          </button>
        </div>
      </div>
    </Modal>

    <Modal :open="joining" title="Enter a code" @close="joining = false">
      <label class="block text-[12px] text-[var(--color-muted)]" for="join-code">Pairing code</label>
      <input
        id="join-code"
        v-model="joinCode"
        class="mono mt-1 mb-3 w-full rounded-md border border-[var(--color-line)] bg-[var(--color-canvas)] px-3 py-2"
        placeholder="12-3456-7890"
        autocomplete="off"
      />
      <label class="block text-[12px] text-[var(--color-muted)]" for="join-addr">
        Address (Tailscale or VPN; leave blank on the same LAN)
      </label>
      <input
        id="join-addr"
        v-model="joinAddr"
        class="mt-1 mb-4 w-full rounded-md border border-[var(--color-line)] bg-[var(--color-canvas)] px-3 py-2"
        placeholder="my-mac:47321 or 100.x.y.z:47321"
      />
      <label class="mb-4 flex items-start gap-2">
        <input v-model="allowManage" type="checkbox" class="mt-1" />
        <span>
          Let the other device browse this computer and set up sync on it
          <span class="block text-[12px] text-[var(--color-muted)]">
            You can change this later in Peers.
          </span>
        </span>
      </label>
      <div class="flex justify-end gap-2">
        <button type="button" class="rounded-md px-2.5 py-1" @click="joining = false">Cancel</button>
        <button
          type="button"
          class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)] disabled:opacity-60"
          :disabled="busy || !joinCode.trim()"
          @click="joinPair"
        >
          Pair
        </button>
      </div>
    </Modal>

    <Modal :open="adding" title="Add a peer" @close="adding = false">
      <label class="block text-[12px] text-[var(--color-muted)]" for="peer-name">Name</label>
      <input
        id="peer-name"
        v-model="name"
        class="mt-1 mb-3 w-full rounded-md border border-[var(--color-line)] bg-[var(--color-canvas)] px-3 py-2"
        placeholder="Desktop"
      />
      <label class="block text-[12px] text-[var(--color-muted)]" for="peer-id">Device id</label>
      <input
        id="peer-id"
        v-model="deviceId"
        class="mono mt-1 mb-3 w-full rounded-md border border-[var(--color-line)] bg-[var(--color-canvas)] px-3 py-2"
        placeholder="64-character hex id from the other device"
      />
      <label class="block text-[12px] text-[var(--color-muted)]" for="peer-addr">Address</label>
      <input
        id="peer-addr"
        v-model="address"
        class="mt-1 mb-4 w-full rounded-md border border-[var(--color-line)] bg-[var(--color-canvas)] px-3 py-2"
        placeholder="192.168.1.20:47321"
      />
      <div class="flex justify-end gap-2">
        <button type="button" class="rounded-md px-2.5 py-1" @click="adding = false">Cancel</button>
        <button
          type="button"
          class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)] disabled:opacity-60"
          :disabled="busy"
          @click="addPeer"
        >
          Add
        </button>
      </div>
    </Modal>

    <Modal :open="!!confirmName" title="Remove peer?" @close="confirmName = null">
      <p>
        Remove <strong>{{ confirmName }}</strong>? They will stop syncing with this device. You can
        add them again later.
      </p>
      <div class="mt-4 flex justify-end gap-2">
        <button type="button" class="rounded-md px-2.5 py-1" @click="confirmName = null">Cancel</button>
        <button
          type="button"
          class="rounded-md bg-red-600 px-2.5 py-1 text-white disabled:opacity-60"
          :disabled="busy"
          @click="removePeer"
        >
          Remove
        </button>
      </div>
    </Modal>
  </div>
</template>
