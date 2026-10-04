<script setup lang="ts">
import { computed, onMounted, ref, watch } from "vue";
import ErrorBanner from "../components/ErrorBanner.vue";
import UpdateStatus from "../components/UpdateStatus.vue";
import { api } from "../lib/api";
import { checkForUpdates, updateActive } from "../lib/updateProgress";
import type { CliShell, CliStatus, Settings } from "../lib/types";

const props = defineProps<{
  version: string;
  mobile: boolean;
}>();

const settings = ref<Settings>({ startAtLogin: true, autoUpdate: true });
const cli = ref<CliStatus | null>(null);
const loading = ref(true);
const error = ref<string | null>(null);
const cliMessage = ref<string | null>(null);
const busy = ref(false);
const checking = computed(() => updateActive.value);
const selectedShell = ref<CliShell>("zsh");
/** macOS only: whether devices allowed to manage this Mac can read every folder. */
const fullDiskAccess = ref<boolean | null>(null);

async function openFullDiskAccess() {
  error.value = null;
  try {
    await api.openFullDiskAccess();
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  }
}

async function recheckFullDiskAccess() {
  fullDiskAccess.value = await api.fullDiskAccess();
}

const selectedHint = computed(() => {
  const hints = cli.value?.shellHints ?? [];
  return hints.find((h) => h.shell === selectedShell.value) ?? null;
});

const displayHint = computed(() => {
  if (cli.value?.onPath) return null;
  return selectedHint.value?.hint ?? cli.value?.hint ?? null;
});

const cliButtonLabel = computed(() => {
  if (!cli.value?.installPath) return "Install";
  if (cli.value.onPath) return "Reinstall";
  return "Add to PATH";
});

const showShellPicker = computed(
  () => !!cli.value && !cli.value.onPath && (cli.value.shellHints?.length ?? 0) > 0,
);

async function load() {
  loading.value = true;
  error.value = null;
  try {
    if (props.mobile) {
      settings.value = await api.getSettings();
      return;
    }
    const [s, c, fda] = await Promise.all([
      api.getSettings(),
      api.cliStatus(),
      api.fullDiskAccess(),
    ]);
    settings.value = s;
    cli.value = c;
    fullDiskAccess.value = fda;
    if (c.detectedShell) {
      selectedShell.value = c.detectedShell;
    }
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  } finally {
    loading.value = false;
  }
}

async function toggle(key: "startAtLogin" | "autoUpdate", value: boolean) {
  settings.value = { ...settings.value, [key]: value };
  try {
    settings.value = await api.setSettings({ [key]: value });
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
    await load();
  }
}

async function checkUpdates() {
  busy.value = true;
  error.value = null;
  try {
    await checkForUpdates();
  } finally {
    busy.value = false;
  }
}

async function installCli() {
  busy.value = true;
  error.value = null;
  try {
    const shell = showShellPicker.value ? selectedShell.value : undefined;
    const result = await api.installCli(shell);
    cliMessage.value = result.message;
    cli.value = await api.cliStatus();
    if (result.detectedShell) {
      selectedShell.value = result.detectedShell;
    }
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  } finally {
    busy.value = false;
  }
}

watch(selectedShell, () => {
  cliMessage.value = null;
});

onMounted(load);
defineExpose({ load });
</script>

<template>
  <div>
    <h2 class="mb-3 text-[15px] font-semibold">Settings</h2>
    <ErrorBanner :message="error" />
    <p v-if="loading" class="text-[var(--color-muted)]">Loading settings…</p>
    <div v-else class="space-y-4">
      <p v-if="mobile" class="text-[var(--color-muted)]">
        Relay syncs while this app is open. Version {{ props.version }}.
      </p>
      <section
        v-if="fullDiskAccess !== null"
        class="rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)] px-3 py-2"
      >
        <div class="flex items-center justify-between gap-3">
          <div>
            <p class="font-medium">Access to all folders</p>
            <p class="text-[12px] text-[var(--color-muted)]">
              <template v-if="fullDiskAccess">
                Relay has Full Disk Access. Devices you allowed can browse Desktop, Documents, and
                Downloads without a prompt on this Mac.
              </template>
              <template v-else>
                Without Full Disk Access, browsing Desktop, Documents, or Downloads from another
                device waits on a prompt on this Mac. Turn it on for Relay, then check again.
              </template>
            </p>
          </div>
          <div class="flex shrink-0 gap-2">
            <button
              v-if="!fullDiskAccess"
              type="button"
              class="rounded-md border border-[var(--color-line)] px-2.5 py-1"
              @click="openFullDiskAccess"
            >
              Open settings
            </button>
            <button
              type="button"
              class="rounded-md border border-[var(--color-line)] px-2.5 py-1"
              @click="recheckFullDiskAccess"
            >
              Check again
            </button>
          </div>
        </div>
      </section>
      <section
        v-if="!mobile"
        class="divide-y divide-[var(--color-line)] overflow-hidden rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)]"
      >
        <label class="flex items-center justify-between gap-3 px-3 py-2">
          <span class="font-medium">Start at login</span>
          <input
            type="checkbox"
            :checked="settings.startAtLogin"
            @change="toggle('startAtLogin', ($event.target as HTMLInputElement).checked)"
          />
        </label>
        <label class="flex items-center justify-between gap-3 px-3 py-2">
          <span class="font-medium">Autoupdate</span>
          <input
            type="checkbox"
            :checked="settings.autoUpdate"
            @change="toggle('autoUpdate', ($event.target as HTMLInputElement).checked)"
          />
        </label>
      </section>

      <section
        v-if="!mobile"
        class="rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)] px-3 py-2"
      >
        <div class="flex items-center justify-between gap-3">
          <div>
            <p class="font-medium">Updates</p>
            <p class="text-[12px] text-[var(--color-muted)]">
              Version {{ props.version }}. Checked at startup and every 15 minutes.
            </p>
          </div>
          <button
            type="button"
            class="rounded-md border border-[var(--color-line)] px-2.5 py-1 disabled:opacity-50"
            :disabled="busy || checking"
            @click="checkUpdates"
          >
            Check for updates
          </button>
        </div>
        <UpdateStatus class="mt-3" />
      </section>

      <section
        v-if="!mobile"
        class="rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)] px-3 py-2"
      >
        <div class="flex items-center justify-between gap-3">
          <div>
            <p class="font-medium">Command-line tool</p>
            <p class="text-[12px] text-[var(--color-muted)]">
              <template v-if="cli?.onPath">The `relay` command is available in new terminals.</template>
              <template v-else-if="cli?.installPath">Installed, but not on PATH yet.</template>
              <template v-else>Not installed on this account.</template>
            </p>
          </div>
          <button
            type="button"
            class="rounded-md border border-[var(--color-line)] px-2.5 py-1 disabled:opacity-50"
            :disabled="busy"
            @click="installCli"
          >
            {{ cliButtonLabel }}
          </button>
        </div>
        <div
          v-if="showShellPicker"
          class="mt-2 flex items-center gap-2 text-[12px] text-[var(--color-muted)]"
        >
          <label for="cli-shell" class="shrink-0">Shell</label>
          <select
            id="cli-shell"
            v-model="selectedShell"
            class="rounded-md border border-[var(--color-line)] bg-[var(--color-canvas)] px-2 py-1"
          >
            <option
              v-for="hint in cli?.shellHints ?? []"
              :key="hint.shell"
              :value="hint.shell"
            >
              {{ hint.shell }}
            </option>
          </select>
        </div>
        <p v-if="cliMessage" class="mt-2 text-[12px]">{{ cliMessage }}</p>
        <p v-if="displayHint" class="mono mt-1 text-[12px] text-[var(--color-muted)]">{{ displayHint }}</p>
      </section>

      <button
        v-if="!mobile"
        type="button"
        class="rounded-md border border-[var(--color-line)] px-2.5 py-1"
        @click="api.openLogsFolder()"
      >
        Open logs folder
      </button>
    </div>
  </div>
</template>
