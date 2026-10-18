<script setup lang="ts">
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { openPath, openUrl } from "@tauri-apps/plugin-opener";
import { computed, onActivated, ref, watch } from "vue";
import ErrorBanner from "../components/ErrorBanner.vue";
import UpdateStatus from "../components/UpdateStatus.vue";
import { api, errorText } from "../lib/api";
import { checkForUpdates, updateActive } from "../lib/updateProgress";
import type {
  CliShell,
  CliStatus,
  Collected,
  DropSettings,
  LogLevel,
  Settings,
} from "../lib/types";

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
/** GitHub Releases page the updater downloads from; null when not configured. */
const releasesUrl = ref<string | null>(null);
/** macOS only: whether devices allowed to manage this Mac can read every folder. */
const fullDiskAccess = ref<boolean | null>(null);

async function openFullDiskAccess() {
  error.value = null;
  try {
    await api.openFullDiskAccess();
  } catch (err) {
    error.value = errorText(err);
  }
}

async function recheckFullDiskAccess() {
  error.value = null;
  try {
    fullDiskAccess.value = await api.fullDiskAccess();
  } catch (err) {
    error.value = errorText(err);
  }
}

async function openReleases() {
  if (!releasesUrl.value) return;
  error.value = null;
  try {
    await openUrl(releasesUrl.value);
  } catch (err) {
    error.value = errorText(err);
  }
}

async function openLogsFolder() {
  error.value = null;
  try {
    await api.openLogsFolder();
  } catch (err) {
    error.value = errorText(err);
  }
}

/** How long detailed logging stays on before it turns itself off (D56). */
const DETAILED_MINUTES = 60;
const logLevel = ref<LogLevel | null>(null);
const detailed = computed(
  () => !!logLevel.value && logLevel.value.filter !== logLevel.value.default,
);
const detailedUntil = computed(() => {
  const until = logLevel.value?.until_ms;
  return until ? new Date(until).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }) : null;
});
const diagnostics = ref<Collected | null>(null);
const collecting = ref(false);

async function setDetailed(on: boolean) {
  error.value = null;
  try {
    logLevel.value = await api.setLogLevel(on ? "debug" : null, on ? DETAILED_MINUTES : null);
  } catch (err) {
    error.value = errorText(err);
    logLevel.value = await api.getLogLevel().catch(() => logLevel.value);
  }
}

async function saveDiagnostics(all: boolean) {
  collecting.value = true;
  error.value = null;
  diagnostics.value = null;
  try {
    diagnostics.value = await api.saveDiagnostics(all);
  } catch (err) {
    error.value = errorText(err);
  } finally {
    collecting.value = false;
  }
}

function folderOf(path: string): string {
  const cut = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
  return cut > 0 ? path.slice(0, cut) : path;
}

async function showDiagnostics() {
  const first = diagnostics.value?.bundles[0];
  if (!first) return;
  try {
    await openPath(folderOf(first));
  } catch (err) {
    error.value = errorText(err);
  }
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

let loaded = false;

/** The placeholder shows only until the first load; later loads refresh in place. */
async function load() {
  if (!loaded) loading.value = true;
  error.value = null;
  try {
    logLevel.value = await api.getLogLevel().catch(() => null);
    if (props.mobile) {
      settings.value = await api.getSettings();
      return;
    }
    const [s, c, fda, releases, d] = await Promise.all([
      api.getSettings(),
      api.cliStatus(),
      api.fullDiskAccess(),
      api.releasesUrl(),
      api.getDropSettings(),
    ]);
    settings.value = s;
    drops.value = d;
    cli.value = c;
    fullDiskAccess.value = fda;
    releasesUrl.value = releases;
    if (c.detectedShell) {
      selectedShell.value = c.detectedShell;
    }
  } catch (err) {
    error.value = errorText(err);
  } finally {
    loading.value = false;
    loaded = true;
  }
}

async function toggle(key: "startAtLogin" | "autoUpdate", value: boolean) {
  settings.value = { ...settings.value, [key]: value };
  try {
    settings.value = await api.setSettings({ [key]: value });
  } catch (err) {
    error.value = errorText(err);
    await load();
  }
}

/** Files other devices send here (D50). Null until loaded, and on mobile. */
const drops = ref<DropSettings | null>(null);

async function setDrops(patch: { receive?: boolean; inbox?: string; autoCopy?: boolean }) {
  error.value = null;
  try {
    drops.value = await api.setDropSettings(patch);
  } catch (err) {
    error.value = errorText(err);
    drops.value = await api.getDropSettings().catch(() => drops.value);
  }
}

async function chooseInbox() {
  const selected = await openDialog({
    directory: true,
    multiple: false,
    title: "Folder for files sent to this computer",
    defaultPath: drops.value?.inbox ?? undefined,
  });
  if (typeof selected === "string") await setDrops({ inbox: selected });
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
    error.value = errorText(err);
  } finally {
    busy.value = false;
  }
}

watch(selectedShell, () => {
  cliMessage.value = null;
});

onActivated(load);
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
        v-if="drops"
        class="divide-y divide-[var(--color-line)] overflow-hidden rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)]"
      >
        <label class="flex items-center justify-between gap-3 px-3 py-2">
          <span>
            <span class="block font-medium">Take files from your devices</span>
            <span class="block text-[12px] text-[var(--color-muted)]">
              A paired phone or computer can send a photo or file here. It pops up in the corner,
              ready to copy or drag.
            </span>
          </span>
          <input
            type="checkbox"
            :checked="drops.receive"
            @change="setDrops({ receive: ($event.target as HTMLInputElement).checked })"
          />
        </label>
        <div class="flex items-center justify-between gap-3 px-3 py-2">
          <div class="min-w-0">
            <p class="font-medium">Save them in</p>
            <p class="mono truncate text-[12px] text-[var(--color-muted)]" :title="drops.inbox ?? ''">
              {{ drops.inbox ?? "No folder" }}
            </p>
          </div>
          <div class="flex shrink-0 gap-2">
            <button
              v-if="drops.inboxSet"
              type="button"
              class="rounded-md border border-[var(--color-line)] px-2.5 py-1"
              @click="setDrops({ inbox: '' })"
            >
              Use default
            </button>
            <button
              type="button"
              class="rounded-md border border-[var(--color-line)] px-2.5 py-1"
              @click="chooseInbox"
            >
              Choose…
            </button>
          </div>
        </div>
        <label class="flex items-center justify-between gap-3 px-3 py-2">
          <span class="font-medium">Copy each one to the clipboard as it arrives</span>
          <input
            type="checkbox"
            :checked="drops.autoCopy"
            @change="setDrops({ autoCopy: ($event.target as HTMLInputElement).checked })"
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
              Version {{ props.version }}. Checked at startup and every 5 minutes.
              <a
                v-if="releasesUrl"
                :href="releasesUrl"
                class="text-[var(--color-accent)] underline"
                @click.prevent="openReleases"
                >All releases</a
              >
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

      <section
        class="divide-y divide-[var(--color-line)] overflow-hidden rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)]"
      >
        <label class="flex items-center justify-between gap-3 px-3 py-2">
          <span>
            <span class="block font-medium">Detailed logging</span>
            <span class="block text-[12px] text-[var(--color-muted)]">
              <template v-if="detailed && detailedUntil">On until {{ detailedUntil }}.</template>
              <template v-else-if="detailed">On until Relay restarts.</template>
              <template v-else>
                Records every connection attempt for an hour, then turns itself off.
              </template>
            </span>
          </span>
          <input
            type="checkbox"
            :checked="detailed"
            :disabled="!logLevel"
            @change="setDetailed(($event.target as HTMLInputElement).checked)"
          />
        </label>
        <div v-if="!mobile" class="px-3 py-2">
          <div class="flex items-center justify-between gap-3">
            <div>
              <p class="font-medium">Diagnostics</p>
              <p class="text-[12px] text-[var(--color-muted)]">
                Logs and a network report in Downloads. All my devices adds every connected device
                that lets this one manage it, and a merged timeline.
              </p>
            </div>
            <div class="flex shrink-0 gap-2">
              <button
                type="button"
                class="rounded-md border border-[var(--color-line)] px-2.5 py-1 disabled:opacity-50"
                :disabled="collecting"
                @click="saveDiagnostics(false)"
              >
                This device
              </button>
              <button
                type="button"
                class="rounded-md border border-[var(--color-line)] px-2.5 py-1 disabled:opacity-50"
                :disabled="collecting"
                @click="saveDiagnostics(true)"
              >
                All my devices
              </button>
            </div>
          </div>
          <p v-if="collecting" class="mt-2 text-[12px] text-[var(--color-muted)]">Collecting…</p>
          <div v-if="diagnostics" class="mt-2 text-[12px]">
            <p>
              Saved {{ diagnostics.bundles.length }}
              {{ diagnostics.bundles.length === 1 ? "bundle" : "bundles" }}<template
                v-if="diagnostics.timeline"
              >
                and a timeline</template
              >.
              <a
                href="#"
                class="text-[var(--color-accent)] underline"
                @click.prevent="showDiagnostics"
                >Show</a
              >
            </p>
            <p v-if="diagnostics.nobody_to_pull" class="text-[var(--color-muted)]">
              No connected device lets this one manage it, so only this device is included.
            </p>
            <p v-for="failure in diagnostics.failures" :key="failure" class="text-[var(--color-muted)]">
              {{ failure }}
            </p>
          </div>
        </div>
        <p v-else class="px-3 py-2 text-[12px] text-[var(--color-muted)]">
          To collect this phone's logs, choose Diagnostics, All my devices on a computer this phone
          lets manage it.
        </p>
        <div v-if="!mobile" class="px-3 py-2">
          <button
            type="button"
            class="rounded-md border border-[var(--color-line)] px-2.5 py-1"
            @click="openLogsFolder"
          >
            Open logs folder
          </button>
        </div>
      </section>
    </div>
  </div>
</template>
