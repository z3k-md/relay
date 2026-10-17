<script setup lang="ts">
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { computed, onMounted, onUnmounted, ref } from "vue";
import ErrorBanner from "./components/ErrorBanner.vue";
import Sheet from "./components/Sheet.vue";
import StatusDot from "./components/StatusDot.vue";
import UpdateStatus from "./components/UpdateStatus.vue";
import ActivityView from "./views/ActivityView.vue";
import AddDeviceSheet from "./views/AddDeviceSheet.vue";
import ConflictsView from "./views/ConflictsView.vue";
import DeviceSheet from "./views/DeviceSheet.vue";
import FolderSheet from "./views/FolderSheet.vue";
import HomeView from "./views/HomeView.vue";
import SettingsView from "./views/SettingsView.vue";
import SetupView from "./views/SetupView.vue";
import { api, copyText, errorText } from "./lib/api";
import { folders, home, loadHome, pendingHolds, type Tone } from "./lib/home";
import {
  installUpdate as runInstallUpdate,
  listenForUpdateProgress,
  stopUpdateProgressListener,
  updateActive,
} from "./lib/updateProgress";
import type { ActivityItem, Overview, RunnerState, TransferLive, UpdateAvailable } from "./lib/types";

/** Everything beyond the home screen opens as a sheet over it; sheets stack. */
type SheetView =
  | { kind: "folder"; space: string; mount: string }
  | { kind: "device"; peer: string }
  | { kind: "addDevice" }
  | { kind: "conflicts" }
  | { kind: "activity" }
  | { kind: "settings" };

const overview = ref<Overview | null>(null);
const transfers = ref<TransferLive[]>([]);
const loading = ref(true);
const error = ref<string | null>(null);
const update = ref<UpdateAvailable | null>(null);
const sheets = ref<SheetView[]>([]);
const activityRef = ref<{ prepend: (item: ActivityItem) => void } | null>(null);
const copied = ref(false);

const top = computed(() => sheets.value[sheets.value.length - 1] ?? null);

function setActivity(el: unknown) {
  activityRef.value = el as typeof activityRef.value;
}

function openSheet(view: SheetView) {
  sheets.value = [...sheets.value, view];
}

function closeSheet() {
  sheets.value = sheets.value.slice(0, -1);
  void loadHome();
}

const unlistens: UnlistenFn[] = [];
/** How often the home screen is reloaded on its own. */
const POLL_MS = 5000;
let pollTimer: number | undefined;
let polling = false;
let refreshGen = 0;

const refreshKinds = new Set([
  "peerConnected",
  "peerDisconnected",
  "reloading",
  "pair",
  "started",
  "deletesHeld",
  "conflict",
]);

async function refresh() {
  const gen = ++refreshGen;
  try {
    const next = await api.getOverview();
    if (gen !== refreshGen) return;
    overview.value = next;
    error.value = null;
    if (next.initialized) await loadHome();
  } catch (err) {
    if (gen !== refreshGen) return;
    error.value = errorText(err);
  } finally {
    if (gen === refreshGen) loading.value = false;
  }
}

/** Reload, then again after a pause: calls never pile up on a slow load. */
async function poll() {
  pollTimer = undefined;
  await refresh();
  if (polling) pollTimer = window.setTimeout(poll, POLL_MS);
}

async function createDevice(name: string) {
  loading.value = true;
  error.value = null;
  try {
    overview.value = await api.initDevice(name);
    await loadHome();
  } catch (err) {
    error.value = errorText(err);
  } finally {
    loading.value = false;
  }
}

async function setRunner(action: () => Promise<RunnerState>) {
  try {
    const runner = await action();
    if (overview.value) overview.value = { ...overview.value, runner };
  } catch (err) {
    error.value = errorText(err);
  }
}

const runner = computed(() => overview.value?.runner ?? null);
const paused = computed(() => runner.value?.kind === "paused" || runner.value?.kind === "stopped");
const canToggle = computed(() => {
  const kind = runner.value?.kind;
  return kind === "running" || kind === "starting" || kind === "paused" || kind === "stopped" || kind === "error";
});

/** The one line at the top: is everything synced, and if not, why. */
const status = computed<{ text: string; tone: Tone }>(() => {
  const r = runner.value;
  if (!r) return { text: "Starting", tone: "off" };
  if (r.kind === "error") return { text: r.message, tone: "warn" };
  if (r.kind === "externalService") return { text: r.message, tone: "ok" };
  if (r.kind === "starting") return { text: "Starting", tone: "sync" };
  if (paused.value) return { text: "Paused", tone: "warn" };
  const attention =
    pendingHolds().length + home.offers.length + new Set(home.conflicts.map((c) => `${c.space}/${c.mount}`)).size;
  if (attention) {
    return { text: attention === 1 ? "1 thing needs you" : `${attention} things need you`, tone: "warn" };
  }
  const moving = transfers.value.filter((t) => t.direction !== "index");
  if (moving.length) {
    const first = moving[0];
    const verb = first.direction === "receive" ? "Receiving from" : "Sending to";
    return { text: `${verb} ${first.peerName}`, tone: "sync" };
  }
  if (transfers.value.length) return { text: "Indexing", tone: "sync" };
  const shared = folders().some((f) => f.space.sharedWith.length > 0);
  if (shared && !home.peers.some((p) => p.connected)) return { text: "No devices online", tone: "off" };
  return { text: "Up to date", tone: "ok" };
});

async function installUpdate() {
  error.value = null;
  await runInstallUpdate();
}

async function copyId() {
  if (!overview.value?.deviceId) return;
  await copyText(overview.value.deviceId);
  copied.value = true;
  window.setTimeout(() => (copied.value = false), 1500);
}

onMounted(async () => {
  unlistens.push(
    await listen<RunnerState>("relay://state", (event) => {
      if (overview.value) overview.value = { ...overview.value, runner: event.payload };
    }),
  );
  unlistens.push(
    await listen<ActivityItem>("relay://activity", (event) => {
      activityRef.value?.prepend(event.payload);
      if (refreshKinds.has(event.payload.kind)) void refresh();
    }),
  );
  unlistens.push(
    await listen<TransferLive[]>("relay://transfers", (event) => {
      transfers.value = event.payload;
    }),
  );
  unlistens.push(
    await listen<UpdateAvailable | null>("relay://update-available", (event) => {
      update.value = event.payload;
    }),
  );
  const pending = await api.pendingUpdate().catch(() => null);
  if (pending) update.value = pending;
  await listenForUpdateProgress();
  polling = true;
  pollTimer = window.setTimeout(poll, POLL_MS);
  await refresh();
});

onUnmounted(() => {
  polling = false;
  if (pollTimer !== undefined) window.clearTimeout(pollTimer);
  stopUpdateProgressListener();
  for (const off of unlistens) off();
});
</script>

<template>
  <div class="flex h-full min-h-0 flex-col bg-[var(--color-canvas)] text-[var(--color-ink)]">
    <template v-if="loading && !overview">
      <div class="flex flex-1 items-center justify-center text-[var(--color-muted)]">Starting Relay…</div>
    </template>
    <template v-else-if="overview && !overview.initialized">
      <ErrorBanner class="m-4" :message="error" />
      <SetupView :suggested-name="overview.suggestedName" @create="createDevice" />
    </template>
    <template v-else-if="overview">
      <header class="border-b border-[var(--color-line)] bg-[var(--color-panel)]">
        <div class="mx-auto flex w-full max-w-3xl items-center gap-3 px-4 py-3">
          <div class="min-w-0 flex-1">
            <h1 class="truncate text-[17px] font-semibold tracking-tight">{{ overview.deviceName }}</h1>
            <p class="flex min-w-0 items-center gap-2 text-[13px] text-[var(--color-muted)]">
              <StatusDot :tone="status.tone" />
              <span class="truncate" :title="status.text">{{ status.text }}</span>
            </p>
          </div>
          <button
            type="button"
            class="hidden rounded-md px-2 py-1 text-[var(--color-muted)] hover:text-[var(--color-ink)] min-[520px]:block"
            @click="openSheet({ kind: 'activity' })"
          >
            Recent activity
          </button>
          <button
            v-if="canToggle"
            type="button"
            class="rounded-md border border-[var(--color-line)] px-2.5 py-1"
            @click="setRunner(paused ? api.resumeSync : api.pauseSync)"
          >
            {{ paused ? "Resume" : "Pause" }}
          </button>
          <button
            type="button"
            class="rounded-md px-2 py-1 text-[18px] leading-none text-[var(--color-muted)] hover:text-[var(--color-ink)]"
            aria-label="Settings"
            title="Settings"
            @click="openSheet({ kind: 'settings' })"
          >
            ⚙
          </button>
        </div>
      </header>
      <main class="min-h-0 flex-1 overflow-auto px-4 py-5">
        <ErrorBanner :message="error" />
        <UpdateStatus class="mx-auto mb-4 max-w-3xl" />
        <HomeView
          :transfers="transfers"
          :update="update && !updateActive ? update : null"
          :mobile="overview.mobile"
          @folder="(space, mount) => openSheet({ kind: 'folder', space, mount })"
          @device="(peer) => openSheet({ kind: 'device', peer })"
          @add-device="openSheet({ kind: 'addDevice' })"
          @conflicts="openSheet({ kind: 'conflicts' })"
          @install-update="installUpdate"
        />
      </main>

      <div v-for="(view, i) in sheets" v-show="view === top" :key="i">
        <FolderSheet
          v-if="view.kind === 'folder'"
          :space="view.space"
          :mount="view.mount"
          :this-device="overview.deviceId ?? ''"
          :transfers="transfers"
          @close="closeSheet"
        />
        <DeviceSheet
          v-else-if="view.kind === 'device'"
          :peer="view.peer"
          @close="closeSheet"
          @folder="(space, mount) => openSheet({ kind: 'folder', space, mount })"
        />
        <AddDeviceSheet
          v-else-if="view.kind === 'addDevice'"
          @close="closeSheet"
          @device="(peer) => { sheets = []; openSheet({ kind: 'device', peer }); }"
        />
        <Sheet v-else-if="view.kind === 'conflicts'" title="Conflicts" wide @close="closeSheet">
          <KeepAlive><ConflictsView /></KeepAlive>
        </Sheet>
        <Sheet v-else-if="view.kind === 'activity'" title="Recent activity" @close="closeSheet">
          <KeepAlive><ActivityView :ref="setActivity" /></KeepAlive>
        </Sheet>
        <Sheet v-else-if="view.kind === 'settings'" title="Settings" @close="closeSheet">
          <section class="mb-4 rounded-lg border border-[var(--color-line)] px-3 py-2.5">
            <p class="font-medium">This device</p>
            <p class="text-[12px] text-[var(--color-muted)]">{{ overview.deviceName }} · Relay {{ overview.version }}</p>
            <div class="mt-2 flex items-start gap-2">
              <code class="mono min-w-0 flex-1 break-all text-[12px]">{{ overview.deviceId }}</code>
              <button
                type="button"
                class="shrink-0 rounded-md border border-[var(--color-line)] px-2 py-0.5 text-[12px]"
                @click="copyId"
              >
                {{ copied ? "Copied" : "Copy id" }}
              </button>
            </div>
            <button
              type="button"
              class="mt-2 text-[12px] text-[var(--color-accent)] underline min-[520px]:hidden"
              @click="openSheet({ kind: 'activity' })"
            >
              Recent activity
            </button>
          </section>
          <KeepAlive>
            <SettingsView :version="overview.version" :mobile="overview.mobile" />
          </KeepAlive>
        </Sheet>
      </div>
    </template>
    <template v-else>
      <div class="p-6">
        <ErrorBanner :message="error ?? 'Relay could not load this device.'" />
        <button type="button" class="rounded-md border border-[var(--color-line)] px-2.5 py-1" @click="refresh">
          Try again
        </button>
      </div>
    </template>
  </div>
</template>
