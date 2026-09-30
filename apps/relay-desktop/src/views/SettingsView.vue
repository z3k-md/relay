<script setup lang="ts">
import { onMounted, ref } from "vue";
import ErrorBanner from "../components/ErrorBanner.vue";
import { api } from "../lib/api";
import type { CliStatus, Settings, UpdateInfo } from "../lib/types";

const props = defineProps<{
  version: string;
}>();

const settings = ref<Settings>({ startAtLogin: true, autoUpdate: true });
const cli = ref<CliStatus | null>(null);
const loading = ref(true);
const error = ref<string | null>(null);
const updateResult = ref<UpdateInfo | null>(null);
const cliMessage = ref<string | null>(null);
const busy = ref(false);

async function load() {
  loading.value = true;
  error.value = null;
  try {
    const [s, c] = await Promise.all([api.getSettings(), api.cliStatus()]);
    settings.value = s;
    cli.value = c;
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
    updateResult.value = await api.checkForUpdates();
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  } finally {
    busy.value = false;
  }
}

async function installCli() {
  busy.value = true;
  error.value = null;
  try {
    const result = await api.installCli();
    cliMessage.value = result.message;
    cli.value = await api.cliStatus();
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
    <h2 class="mb-3 text-[15px] font-semibold">Settings</h2>
    <ErrorBanner :message="error" />
    <p v-if="loading" class="text-[var(--color-muted)]">Loading settings…</p>
    <div v-else class="space-y-4">
      <label class="flex items-center justify-between gap-3 rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)] px-3 py-2">
        <span>
          <span class="block font-medium">Start at login</span>
          <span class="text-[12px] text-[var(--color-muted)]">Launch Relay in the tray when you sign in.</span>
        </span>
        <input
          type="checkbox"
          class="h-4 w-4"
          :checked="settings.startAtLogin"
          @change="toggle('startAtLogin', ($event.target as HTMLInputElement).checked)"
        />
      </label>
      <label class="flex items-center justify-between gap-3 rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)] px-3 py-2">
        <span>
          <span class="block font-medium">Automatically install updates</span>
          <span class="text-[12px] text-[var(--color-muted)]">Download and restart when a new build is published.</span>
        </span>
        <input
          type="checkbox"
          class="h-4 w-4"
          :checked="settings.autoUpdate"
          @change="toggle('autoUpdate', ($event.target as HTMLInputElement).checked)"
        />
      </label>

      <section class="rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)] px-3 py-2">
        <div class="flex items-center justify-between gap-3">
          <div>
            <p class="font-medium">Updates</p>
            <p class="text-[12px] text-[var(--color-muted)]">Version {{ props.version }}</p>
          </div>
          <button
            type="button"
            class="rounded-md border border-[var(--color-line)] px-2.5 py-1"
            :disabled="busy"
            @click="checkUpdates"
          >
            Check for updates
          </button>
        </div>
        <p v-if="updateResult" class="mt-2 text-[12px] text-[var(--color-muted)]">
          {{ updateResult.message }}
        </p>
      </section>

      <section class="rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)] px-3 py-2">
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
            class="rounded-md border border-[var(--color-line)] px-2.5 py-1"
            :disabled="busy"
            @click="installCli"
          >
            Install
          </button>
        </div>
        <p v-if="cliMessage" class="mt-2 text-[12px]">{{ cliMessage }}</p>
        <p v-if="cli?.hint" class="mono mt-1 text-[12px] text-[var(--color-muted)]">{{ cli.hint }}</p>
      </section>

      <button
        type="button"
        class="rounded-md border border-[var(--color-line)] px-2.5 py-1"
        @click="api.openLogsFolder()"
      >
        Open logs folder
      </button>
    </div>
  </div>
</template>
