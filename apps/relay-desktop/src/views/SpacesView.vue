<script setup lang="ts">
import { open } from "@tauri-apps/plugin-dialog";
import { onMounted, ref } from "vue";
import EmptyState from "../components/EmptyState.vue";
import ErrorBanner from "../components/ErrorBanner.vue";
import Modal from "../components/Modal.vue";
import { api } from "../lib/api";
import type { OfferView, PeerView, SpaceView } from "../lib/types";

const spaces = ref<SpaceView[]>([]);
const offers = ref<OfferView[]>([]);
const peers = ref<PeerView[]>([]);
const loading = ref(true);
const error = ref<string | null>(null);
const creating = ref(false);
const spaceName = ref("");
const shareFor = ref<string | null>(null);
const sharePeer = ref("");
const busy = ref(false);

async function load() {
  loading.value = true;
  error.value = null;
  try {
    const [s, o, p] = await Promise.all([api.listSpaces(), api.listOffers(), api.listPeers()]);
    spaces.value = s;
    offers.value = o;
    peers.value = p;
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  } finally {
    loading.value = false;
  }
}

function folderName(path: string): string {
  const parts = path.replace(/\\/g, "/").split("/").filter(Boolean);
  return parts[parts.length - 1] ?? "folder";
}

async function createSpace() {
  busy.value = true;
  error.value = null;
  try {
    await api.createSpace(spaceName.value.trim());
    creating.value = false;
    spaceName.value = "";
    await load();
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  } finally {
    busy.value = false;
  }
}

async function addFolder(space: string) {
  error.value = null;
  const selected = await open({ directory: true, multiple: false, title: "Choose a folder to sync" });
  if (!selected || typeof selected !== "string") return;
  const mount = folderName(selected);
  busy.value = true;
  try {
    const added = await api.addMount(space, mount, selected);
    const target = spaces.value.find((s) => s.name === space);
    if (target && !target.mounts.some((m) => m.name === added.name)) {
      target.mounts.push(added);
    }
    await load();
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  } finally {
    busy.value = false;
  }
}

async function doShare() {
  if (!shareFor.value || !sharePeer.value) return;
  const spaceName = shareFor.value;
  const peerName = sharePeer.value;
  busy.value = true;
  error.value = null;
  try {
    await api.share(spaceName, peerName);
    shareFor.value = null;
    const target = spaces.value.find((s) => s.name === spaceName);
    if (target && !target.sharedWith.includes(peerName)) {
      target.sharedWith.push(peerName);
    }
    await load();
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  } finally {
    busy.value = false;
  }
}

async function doUnshare(space: string, peer: string) {
  busy.value = true;
  error.value = null;
  try {
    await api.unshare(space, peer);
    await load();
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  } finally {
    busy.value = false;
  }
}

async function joinOffer(offer: OfferView) {
  error.value = null;
  const selected = await open({
    directory: true,
    multiple: false,
    title: `Choose a local folder for ${offer.name}`,
  });
  if (!selected || typeof selected !== "string") return;
  busy.value = true;
  try {
    await api.joinSpace(offer.name, offer.peer);
    if (offer.mounts.length === 1) {
      await api.addMount(offer.name, offer.mounts[0].name, selected);
    } else if (offer.mounts.length > 1) {
      await api.addMount(offer.name, offer.mounts[0].name, selected);
    }
    await load();
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  } finally {
    busy.value = false;
  }
}

function unusedPeers(space: SpaceView): PeerView[] {
  return peers.value.filter((p) => !space.sharedWith.includes(p.name));
}

onMounted(load);
defineExpose({ load });
</script>

<template>
  <div>
    <div class="mb-3 flex items-center justify-between">
      <h2 class="text-[15px] font-semibold">Spaces</h2>
      <button
        type="button"
        class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)]"
        @click="creating = true"
      >
        Create space
      </button>
    </div>
    <ErrorBanner :message="error" />
    <p v-if="loading" class="text-[var(--color-muted)]">Loading spaces…</p>
    <template v-else>
      <section v-if="offers.length" class="mb-4">
        <h3 class="mb-2 text-[13px] font-semibold">Offers from peers</h3>
        <ul class="space-y-2">
          <li
            v-for="offer in offers"
            :key="`${offer.peer}-${offer.name}`"
            class="flex items-center justify-between gap-3 rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)] px-3 py-2"
          >
            <div>
              <p class="font-medium">{{ offer.name }}</p>
              <p class="text-[12px] text-[var(--color-muted)]">
                From {{ offer.peer }}
                <span v-if="offer.mounts.length">
                  · {{ offer.mounts.map((m) => m.name).join(", ") }}
                </span>
              </p>
            </div>
            <button
              type="button"
              class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)]"
              :disabled="busy"
              @click="joinOffer(offer)"
            >
              Join
            </button>
          </li>
        </ul>
      </section>

      <EmptyState
        v-if="spaces.length === 0"
        title="No spaces yet"
        body="A space is a named collection of folders you sync — for example Code or Game mods. Create one, add a folder, then share it with a peer."
      >
        <button
          type="button"
          class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)]"
          @click="creating = true"
        >
          Create space
        </button>
      </EmptyState>

      <ul v-else class="space-y-3">
        <li
          v-for="space in spaces"
          :key="space.id"
          class="rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)] p-3"
        >
          <div class="flex items-start justify-between gap-3">
            <div>
              <h3 class="font-semibold">{{ space.name }}</h3>
              <p class="text-[12px] text-[var(--color-muted)]">
                Shared with
                {{ space.sharedWith.length ? space.sharedWith.join(", ") : "nobody yet" }}
              </p>
            </div>
            <div class="flex gap-2">
              <button
                type="button"
                class="rounded-md border border-[var(--color-line)] px-2 py-1"
                @click="addFolder(space.name)"
              >
                Add folder
              </button>
              <button
                type="button"
                class="rounded-md border border-[var(--color-line)] px-2 py-1"
                :disabled="unusedPeers(space).length === 0"
                @click="shareFor = space.name; sharePeer = unusedPeers(space)[0]?.name ?? ''"
              >
                Share
              </button>
            </div>
          </div>
          <ul v-if="space.mounts.length" class="mt-2 space-y-1">
            <li v-for="mount in space.mounts" :key="mount.name" class="text-[13px]">
              <span class="font-medium">{{ mount.name }}</span>
              <span class="text-[var(--color-muted)]">
                — {{ mount.path ?? "not attached on this device" }}
                <span v-if="mount.state && mount.state !== 'OK'"> · {{ mount.state }}</span>
              </span>
            </li>
          </ul>
          <p v-else class="mt-2 text-[12px] text-[var(--color-muted)]">No local folders yet.</p>
          <div v-if="space.sharedWith.length" class="mt-2 flex flex-wrap gap-1">
            <button
              v-for="peer in space.sharedWith"
              :key="peer"
              type="button"
              class="rounded-full border border-[var(--color-line)] px-2 py-0.5 text-[12px] text-[var(--color-muted)]"
              :title="`Stop sharing with ${peer}`"
              @click="doUnshare(space.name, peer)"
            >
              {{ peer }} ✕
            </button>
          </div>
        </li>
      </ul>
    </template>

    <Modal :open="creating" title="Create a space" @close="creating = false">
      <label class="block text-[12px] text-[var(--color-muted)]" for="space-name">Name</label>
      <input
        id="space-name"
        v-model="spaceName"
        class="mt-1 mb-4 w-full rounded-md border border-[var(--color-line)] bg-[var(--color-canvas)] px-3 py-2"
        placeholder="Code"
      />
      <div class="flex justify-end gap-2">
        <button type="button" class="rounded-md px-2.5 py-1" @click="creating = false">Cancel</button>
        <button
          type="button"
          class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)]"
          :disabled="busy"
          @click="createSpace"
        >
          Create
        </button>
      </div>
    </Modal>

    <Modal :open="!!shareFor" title="Share with a peer" @close="shareFor = null">
      <p class="mb-2 text-[var(--color-muted)]">
        {{ shareFor }} will be offered to the peer you pick. They join from their Offers list.
      </p>
      <select
        v-model="sharePeer"
        class="mb-4 w-full rounded-md border border-[var(--color-line)] bg-[var(--color-canvas)] px-3 py-2"
      >
        <option v-for="peer in peers" :key="peer.id" :value="peer.name">{{ peer.name }}</option>
      </select>
      <div class="flex justify-end gap-2">
        <button type="button" class="rounded-md px-2.5 py-1" @click="shareFor = null">Cancel</button>
        <button
          type="button"
          class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)]"
          :disabled="busy || !sharePeer"
          @click="doShare"
        >
          Share
        </button>
      </div>
    </Modal>
  </div>
</template>
