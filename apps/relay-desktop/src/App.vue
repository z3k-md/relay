<script setup lang="ts">
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { onMounted, onUnmounted, ref, watch } from "vue";
import ErrorBanner from "./components/ErrorBanner.vue";
import UpdateStatus from "./components/UpdateStatus.vue";
import ActivityView from "./views/ActivityView.vue";
import BrowseView from "./views/BrowseView.vue";
import FilesView from "./views/FilesView.vue";
import ConflictsView from "./views/ConflictsView.vue";
import OverviewView from "./views/OverviewView.vue";
import PeersView from "./views/PeersView.vue";
import SettingsView from "./views/SettingsView.vue";
import SetupView from "./views/SetupView.vue";
import SpacesView from "./views/SpacesView.vue";
import { api } from "./lib/api";
import {
  installUpdate as runInstallUpdate,
  listenForUpdateProgress,
  stopUpdateProgressListener,
  updateActive,
} from "./lib/updateProgress";
import type {
  ActivityItem,
  Overview,
  Page,
  RunnerState,
  TransferLive,
  UpdateAvailable,
} from "./lib/types";

const pages: { id: Page; label: string }[] = [
  { id: "overview", label: "Overview" },
  { id: "peers", label: "Peers" },
  { id: "spaces", label: "Spaces" },
  { id: "files", label: "Files" },
  { id: "browse", label: "Browse" },
  { id: "conflicts", label: "Conflicts" },
  { id: "activity", label: "Activity" },
  { id: "settings", label: "Settings" },
];

const page = ref<Page>("overview");
const overview = ref<Overview | null>(null);
const transfers = ref<TransferLive[]>([]);
const loading = ref(true);
const error = ref<string | null>(null);
const update = ref<UpdateAvailable | null>(null);
const activityRef = ref<{ prepend: (item: ActivityItem) => void } | null>(null);

const unlistens: UnlistenFn[] = [];
let overviewTimer: number | undefined;
let refreshGen = 0;

const overviewKinds = new Set([
  "peerConnected",
  "peerDisconnected",
  "reloading",
  "pair",
  "started",
]);

async function refresh() {
  const gen = ++refreshGen;
  try {
    const next = await api.getOverview();
    if (gen !== refreshGen) return;
    overview.value = next;
    error.value = null;
  } catch (err) {
    if (gen !== refreshGen) return;
    error.value = err instanceof Error ? err.message : String(err);
  } finally {
    if (gen === refreshGen) loading.value = false;
  }
}

async function createDevice(name: string) {
  loading.value = true;
  error.value = null;
  try {
    overview.value = await api.initDevice(name);
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  } finally {
    loading.value = false;
  }
}

async function pause() {
  try {
    const runner = await api.pauseSync();
    if (overview.value) overview.value = { ...overview.value, runner };
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  }
}

async function resume() {
  try {
    const runner = await api.resumeSync();
    if (overview.value) overview.value = { ...overview.value, runner };
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  }
}

async function installUpdate() {
  error.value = null;
  await runInstallUpdate();
}

onMounted(async () => {
  unlistens.push(
    await listen<RunnerState>("relay://state", (event) => {
      if (overview.value) {
        overview.value = { ...overview.value, runner: event.payload };
      }
    }),
  );
  unlistens.push(
    await listen<ActivityItem>("relay://activity", (event) => {
      activityRef.value?.prepend(event.payload);
      if (overviewKinds.has(event.payload.kind)) void refresh();
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
  overviewTimer = window.setInterval(() => {
    void refresh();
  }, 5000);
  await refresh();
});

watch(page, (next) => {
  if (next === "overview") void refresh();
});

onUnmounted(() => {
  if (overviewTimer !== undefined) window.clearInterval(overviewTimer);
  stopUpdateProgressListener();
  for (const off of unlistens) {
    off();
  }
});
</script>

<template>
  <div class="flex h-full min-h-0 flex-col bg-[var(--color-canvas)] text-[var(--color-ink)]">
    <template v-if="loading && !overview">
      <div class="flex flex-1 items-center justify-center text-[var(--color-muted)]">
        Starting Relay…
      </div>
    </template>
    <template v-else-if="overview && !overview.initialized">
      <ErrorBanner class="m-4" :message="error" />
      <SetupView :suggested-name="overview.suggestedName" @create="createDevice" />
    </template>
    <template v-else-if="overview">
      <header
        class="flex items-center justify-between border-b border-[var(--color-line)] px-4 py-2 min-[800px]:hidden"
      >
        <strong class="tracking-tight">Relay</strong>
        <span class="text-[12px] text-[var(--color-muted)]">{{ overview.deviceName }}</span>
      </header>
      <nav
        class="flex gap-1 overflow-x-auto border-b border-[var(--color-line)] px-2 py-1.5 min-[800px]:hidden"
      >
        <button
          v-for="item in pages"
          :key="item.id"
          type="button"
          class="shrink-0 rounded-md px-2.5 py-1"
          :class="page === item.id ? 'bg-[var(--color-panel)] font-medium' : 'text-[var(--color-muted)]'"
          @click="page = item.id"
        >
          {{ item.label }}
        </button>
      </nav>
      <div class="flex min-h-0 flex-1">
        <aside
          class="hidden w-44 shrink-0 flex-col border-r border-[var(--color-line)] bg-[var(--color-panel)] min-[800px]:flex"
        >
          <div class="px-4 py-3">
            <p class="text-[15px] font-semibold tracking-tight">Relay</p>
            <p class="truncate text-[12px] text-[var(--color-muted)]">{{ overview.deviceName }}</p>
          </div>
          <nav class="flex flex-1 flex-col gap-0.5 px-2 pb-3">
            <button
              v-for="item in pages"
              :key="item.id"
              type="button"
              class="rounded-md px-2.5 py-1.5 text-left"
              :class="page === item.id ? 'bg-[var(--color-canvas)] font-medium' : 'text-[var(--color-muted)]'"
              @click="page = item.id"
            >
              {{ item.label }}
            </button>
          </nav>
        </aside>
        <main class="min-w-0 flex-1 overflow-auto p-4">
          <ErrorBanner :message="error" />
          <div
            v-if="update && !updateActive && !overview.mobile"
            class="mb-4 rounded-lg border border-teal-300/70 bg-teal-50 px-3 py-2 dark:border-teal-900 dark:bg-teal-950/50"
          >
            <div class="flex flex-wrap items-center justify-between gap-2">
              <div>
                <p class="font-medium">Relay {{ update.version }} is available</p>
                <p v-if="update.notes" class="line-clamp-2 text-[var(--color-muted)]">{{ update.notes }}</p>
              </div>
              <button
                type="button"
                class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)]"
                @click="installUpdate"
              >
                Install update
              </button>
            </div>
          </div>
          <UpdateStatus v-if="page !== 'settings'" class="mb-4" />
          <OverviewView
            v-if="page === 'overview'"
            :overview="overview"
            :transfers="transfers"
            @pause="pause"
            @resume="resume"
          />
          <PeersView v-else-if="page === 'peers'" />
          <SpacesView v-else-if="page === 'spaces'" :transfers="transfers" />
          <FilesView v-else-if="page === 'files'" :transfers="transfers" />
          <BrowseView v-else-if="page === 'browse'" />
          <ConflictsView v-else-if="page === 'conflicts'" />
          <ActivityView v-else-if="page === 'activity'" ref="activityRef" />
          <SettingsView
            v-else-if="page === 'settings'"
            :version="overview.version"
            :mobile="overview.mobile"
          />
        </main>
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
