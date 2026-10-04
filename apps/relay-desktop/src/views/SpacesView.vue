<script setup lang="ts">
import { open } from "@tauri-apps/plugin-dialog";
import { openPath } from "@tauri-apps/plugin-opener";
import { computed, onMounted, ref } from "vue";
import EmptyState from "../components/EmptyState.vue";
import ErrorBanner from "../components/ErrorBanner.vue";
import Modal from "../components/Modal.vue";
import { api } from "../lib/api";
import type { MountView, OfferView, PeerView, SpaceView, TransferLive } from "../lib/types";

const props = defineProps<{
  transfers?: TransferLive[];
}>();

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

function indexingLabel(space: string, mount: string): string | null {
  const row = (props.transfers ?? []).find(
    (transfer) =>
      transfer.direction === "index" && transfer.space === space && transfer.mount === mount,
  );
  if (!row) return null;
  const files = row.filesDone.toLocaleString();
  return `Indexing… ${files} files`;
}

function folderName(path: string): string {
  const parts = path.replace(/\\/g, "/").split("/").filter(Boolean);
  return parts[parts.length - 1] ?? "folder";
}

const trimmedSpaceName = computed(() => spaceName.value.trim());

function spaceNameIssue(name: string): string | null {
  if (!name) return "Enter a name.";
  if ([...name].length > 64) return "Use 64 characters or fewer.";
  if (name.includes("/") || name.includes("\\")) return "Name can't include / or \\.";
  if (/\p{Cc}/u.test(name)) return "Name can't include control characters.";
  if (spaces.value.some((space) => space.name === name)) {
    return "A space with this name already exists.";
  }
  return null;
}

const nameIssue = computed(() => spaceNameIssue(trimmedSpaceName.value));
const canCreateSpace = computed(() => nameIssue.value === null);

/** Run one action with the shared busy flag and error banner, then reload. */
async function run(action: () => Promise<unknown>) {
  if (busy.value) return;
  busy.value = true;
  error.value = null;
  try {
    await action();
    await load();
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  } finally {
    busy.value = false;
  }
}

function createSpace() {
  if (!canCreateSpace.value) return;
  return run(async () => {
    await api.createSpace(trimmedSpaceName.value);
    creating.value = false;
    spaceName.value = "";
  });
}

async function chooseFolder(title: string): Promise<string | null> {
  const selected = await open({ directory: true, multiple: false, title });
  return typeof selected === "string" ? selected : null;
}

async function addFolder(space: string) {
  const selected = await chooseFolder("Choose a folder to sync");
  if (selected) await run(() => api.addMount(space, folderName(selected), selected));
}

function doShare() {
  const space = shareFor.value;
  const peer = sharePeer.value;
  if (!space || !peer) return;
  return run(async () => {
    await api.share(space, peer);
    shareFor.value = null;
  });
}

async function openFolder(path: string) {
  error.value = null;
  try {
    await openPath(path);
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  }
}

function doUnshare(space: string, peer: string) {
  return run(() => api.unshare(space, peer));
}

async function joinOffer(offer: OfferView) {
  const selected = await chooseFolder(`Choose a local folder for ${offer.name}`);
  if (!selected) return;
  await run(async () => {
    await api.joinSpace(offer.name, offer.peer);
    const first = offer.mounts[0];
    if (first) await api.addMount(offer.name, first.name, selected);
  });
}

type Confirm = { title: string; body: string; action: string; run: () => Promise<unknown> };
const confirming = ref<Confirm | null>(null);

function confirmRemoveMount(space: string, mount: MountView) {
  confirming.value = {
    title: `Stop syncing ${mount.name}?`,
    body: `This computer stops syncing ${mount.path ?? mount.name}. The files stay where they are, and other devices keep their copies.`,
    action: "Stop syncing",
    run: () => api.removeMount(space, mount.name),
  };
}

function confirmDeleteSpace(space: SpaceView) {
  confirming.value = {
    title: `Delete ${space.name}?`,
    body: "This computer forgets the space and stops sharing it. Other devices keep it, and you can join it again from their offer.",
    action: "Delete space",
    run: () => api.deleteSpace(space.name),
  };
}

function runConfirmed() {
  const pending = confirming.value;
  if (!pending) return;
  return run(async () => {
    await pending.run();
    confirming.value = null;
  });
}

function hasAttachedMount(space: SpaceView): boolean {
  return space.mounts.some((mount) => mount.attached);
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
        body="A space is a set of folders you sync, such as Projects or Documents. Create one, add a folder, then share it with another computer."
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
              <button
                v-if="!hasAttachedMount(space)"
                type="button"
                class="rounded-md border border-[var(--color-line)] px-2 py-1"
                :disabled="busy"
                @click="confirmDeleteSpace(space)"
              >
                Delete
              </button>
            </div>
          </div>
          <ul v-if="space.mounts.length" class="mt-2 space-y-1">
            <li
              v-for="mount in space.mounts"
              :key="mount.name"
              class="flex items-center justify-between gap-2 text-[13px]"
            >
              <div class="min-w-0">
                <span class="font-medium">{{ mount.name }}</span>
                <span class="text-[var(--color-muted)]">
                  — {{ mount.path ?? "not attached on this device" }}
                  <span v-if="indexingLabel(space.name, mount.name)">
                    · {{ indexingLabel(space.name, mount.name) }}
                  </span>
                  <span v-else-if="mount.state && mount.state !== 'OK'"> · {{ mount.state }}</span>
                </span>
              </div>
              <div v-if="mount.path" class="flex shrink-0 gap-1">
                <button
                  type="button"
                  class="rounded-md border border-[var(--color-line)] px-2 py-0.5 text-[12px]"
                  @click="mount.path && openFolder(mount.path)"
                >
                  Open folder
                </button>
                <button
                  type="button"
                  class="rounded-md border border-[var(--color-line)] px-2 py-0.5 text-[12px]"
                  :disabled="busy"
                  @click="confirmRemoveMount(space.name, mount)"
                >
                  Stop syncing
                </button>
              </div>
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
        class="mt-1 w-full rounded-md border border-[var(--color-line)] bg-[var(--color-canvas)] px-3 py-2"
        placeholder="Projects"
        maxlength="64"
        autocomplete="off"
        @keydown.enter="createSpace"
      />
      <p
        class="mt-1 mb-4 min-h-4 text-[12px]"
        :class="trimmedSpaceName && nameIssue ? 'text-[var(--color-danger)]' : 'text-[var(--color-muted)]'"
      >
        {{ nameIssue }}
      </p>
      <div class="flex justify-end gap-2">
        <button type="button" class="rounded-md px-2.5 py-1" @click="creating = false">Cancel</button>
        <button
          type="button"
          class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)] disabled:opacity-50"
          :disabled="busy || !canCreateSpace"
          @click="createSpace"
        >
          Create
        </button>
      </div>
    </Modal>

    <Modal :open="!!confirming" :title="confirming?.title ?? ''" @close="confirming = null">
      <p class="mb-4 text-[var(--color-muted)]">{{ confirming?.body }}</p>
      <div class="flex justify-end gap-2">
        <button type="button" class="rounded-md px-2.5 py-1" @click="confirming = null">Cancel</button>
        <button
          type="button"
          class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)]"
          :disabled="busy"
          @click="runConfirmed"
        >
          {{ confirming?.action }}
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
