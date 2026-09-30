<script setup lang="ts">
import { onMounted, ref } from "vue";
import EmptyState from "../components/EmptyState.vue";
import ErrorBanner from "../components/ErrorBanner.vue";
import Modal from "../components/Modal.vue";
import { api } from "../lib/api";
import type { PeerView } from "../lib/types";

const peers = ref<PeerView[]>([]);
const loading = ref(true);
const error = ref<string | null>(null);
const adding = ref(false);
const name = ref("");
const deviceId = ref("");
const address = ref("");
const confirmName = ref<string | null>(null);
const busy = ref(false);

async function load() {
  loading.value = true;
  error.value = null;
  try {
    peers.value = await api.listPeers();
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  } finally {
    loading.value = false;
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

onMounted(load);
defineExpose({ load });
</script>

<template>
  <div>
    <div class="mb-3 flex items-center justify-between">
      <h2 class="text-[15px] font-semibold">Peers</h2>
      <button
        type="button"
        class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)]"
        @click="adding = true"
      >
        Add peer
      </button>
    </div>
    <ErrorBanner :message="error" />
    <p v-if="loading" class="text-[var(--color-muted)]">Loading peers…</p>
    <EmptyState
      v-else-if="peers.length === 0"
      title="No peers yet"
      body="Add your other computer — a Mac and a Windows PC, for example — using its device id and LAN or Tailscale address."
    >
      <button
        type="button"
        class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)]"
        @click="adding = true"
      >
        Add peer
      </button>
    </EmptyState>
    <ul v-else class="space-y-2">
      <li
        v-for="peer in peers"
        :key="peer.id"
        class="flex items-center justify-between gap-3 rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)] px-3 py-2"
      >
        <div class="min-w-0">
          <div class="flex items-center gap-2">
            <span
              class="inline-block h-2 w-2 rounded-full"
              :class="peer.connected ? 'bg-emerald-500' : 'bg-zinc-400'"
              :title="peer.connected ? 'Online' : 'Offline'"
            />
            <span class="font-medium">{{ peer.name }}</span>
            <span class="mono text-[12px] text-[var(--color-muted)]">{{ peer.shortId }}</span>
          </div>
          <p class="truncate text-[12px] text-[var(--color-muted)]">
            {{ peer.address || "No address" }}
          </p>
        </div>
        <button
          type="button"
          class="text-[var(--color-danger)]"
          @click="confirmName = peer.name"
        >
          Remove
        </button>
      </li>
    </ul>

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
