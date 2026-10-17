<script setup lang="ts">
import { computed, onUnmounted, ref } from "vue";
import ErrorBanner from "../components/ErrorBanner.vue";
import Sheet from "../components/Sheet.vue";
import { api, errorText } from "../lib/api";
import { loadHome } from "../lib/home";

const emit = defineEmits<{
  close: [];
  device: [peer: string];
}>();

type Mode = "choose" | "show" | "enter" | "manual";
const mode = ref<Mode>("choose");
const busy = ref(false);
const error = ref<string | null>(null);
/** Pairing lets the other device set up sync here unless unchecked (D37). */
const allowManage = ref(true);

const code = ref("");
const qr = ref<string | null>(null);
const expires = ref(0);
const now = ref(Date.now());
const paired = ref<string | null>(null);
const failed = ref<string | null>(null);

const joinCode = ref("");
const joinAddr = ref("");
const name = ref("");
const deviceId = ref("");
const address = ref("");

/** How often the pairing status is asked for while a code is shown. */
const PAIR_STATUS_MS = 750;
let statusTimer: number | undefined;
let tickTimer: number | undefined;
let watching = false;

const remaining = computed(() => {
  const ms = expires.value - now.value;
  if (ms <= 0) return "expired";
  const total = Math.ceil(ms / 1000);
  return `${Math.floor(total / 60)}:${(total % 60).toString().padStart(2, "0")}`;
});

function stopWatch() {
  watching = false;
  window.clearTimeout(statusTimer);
  window.clearInterval(tickTimer);
  statusTimer = tickTimer = undefined;
}

/** Ask once, and again after a pause: calls never pile up on a slow host. */
async function watchStatus() {
  statusTimer = undefined;
  try {
    const status = await api.pairStatus();
    if (status.state === "paired") {
      paired.value = status.peerName;
      stopWatch();
      await loadHome();
    } else if (status.state === "failed") {
      failed.value = status.reason;
      stopWatch();
    } else if (status.state === "expired") {
      failed.value = "This code expired. Show a new one.";
      stopWatch();
    }
  } catch (err) {
    failed.value = errorText(err);
    stopWatch();
  }
  if (watching) statusTimer = window.setTimeout(watchStatus, PAIR_STATUS_MS);
}

async function showCode() {
  busy.value = true;
  error.value = null;
  paired.value = failed.value = null;
  try {
    const started = await api.pairStart([], allowManage.value);
    code.value = started.code;
    qr.value = started.qrSvg;
    expires.value = started.expiresAtMs;
    now.value = Date.now();
    mode.value = "show";
    tickTimer = window.setInterval(() => (now.value = Date.now()), 1000);
    watching = true;
    statusTimer = window.setTimeout(watchStatus, PAIR_STATUS_MS);
  } catch (err) {
    error.value = errorText(err);
  } finally {
    busy.value = false;
  }
}

async function cancelCode() {
  stopWatch();
  if (code.value && !paired.value && !failed.value) await api.pairCancel().catch(() => {});
  code.value = "";
  qr.value = null;
}

async function joinWithCode() {
  busy.value = true;
  error.value = null;
  try {
    const result = await api.pairJoin(joinCode.value.trim(), joinAddr.value.trim() || undefined, allowManage.value);
    paired.value = result.peerName;
    await loadHome();
  } catch (err) {
    error.value = errorText(err);
  } finally {
    busy.value = false;
  }
}

async function addManually() {
  busy.value = true;
  error.value = null;
  try {
    const peer = await api.addPeer(name.value.trim(), deviceId.value.trim(), address.value.trim());
    paired.value = peer.name;
    await loadHome();
  } catch (err) {
    error.value = errorText(err);
  } finally {
    busy.value = false;
  }
}

async function close() {
  await cancelCode();
  emit("close");
}

onUnmounted(() => {
  stopWatch();
  if (code.value && !paired.value) api.pairCancel().catch(() => {});
});
</script>

<template>
  <Sheet title="Add a device" @close="close">
    <div class="flex flex-col gap-4">
      <ErrorBanner :message="error" />

      <div v-if="paired" class="flex flex-col items-start gap-3">
        <p class="text-[15px] font-medium">Paired with {{ paired }}</p>
        <p class="text-[var(--color-muted)]">Open a folder and choose Sync to share it with {{ paired }}.</p>
        <button
          type="button"
          class="rounded-md bg-[var(--color-accent)] px-3 py-1.5 text-[var(--color-accent-fg)]"
          @click="emit('device', paired)"
        >
          Open {{ paired }}
        </button>
      </div>

      <template v-else-if="mode === 'choose'">
        <p class="text-[var(--color-muted)]">
          On the same network that is the whole step. Over Tailscale or a VPN, enter the code on
          the other device with this device's address.
        </p>
        <div class="grid gap-2 min-[480px]:grid-cols-2">
          <button
            type="button"
            class="rounded-lg border border-[var(--color-line)] px-3 py-3 text-left hover:border-[var(--color-muted)] disabled:opacity-60"
            :disabled="busy"
            @click="showCode"
          >
            <span class="block font-medium">Show a code</span>
            <span class="block text-[12px] text-[var(--color-muted)]">Scan it with your phone, or type it on the other device.</span>
          </button>
          <button
            type="button"
            class="rounded-lg border border-[var(--color-line)] px-3 py-3 text-left hover:border-[var(--color-muted)]"
            @click="mode = 'enter'"
          >
            <span class="block font-medium">Enter a code</span>
            <span class="block text-[12px] text-[var(--color-muted)]">The other device is showing one.</span>
          </button>
        </div>
        <label class="flex items-start gap-2">
          <input v-model="allowManage" type="checkbox" class="mt-1" />
          <span>
            Let it set up sync on this device
            <span class="block text-[12px] text-[var(--color-muted)]">Right for your own devices. You can change it later.</span>
          </span>
        </label>
        <button type="button" class="self-start text-[12px] text-[var(--color-muted)] underline" @click="mode = 'manual'">
          Add by device id instead
        </button>
      </template>

      <template v-else-if="mode === 'show'">
        <p v-if="failed" class="text-[var(--color-danger)]">{{ failed }}</p>
        <template v-else>
          <p class="text-[var(--color-muted)]">
            Scan this with your phone's camera, or enter the code on the other device. It expires in
            {{ remaining }}.
          </p>
          <!-- eslint-disable-next-line vue/no-v-html -- an SVG this app rendered -->
          <div v-if="qr" class="mx-auto w-[200px] rounded-md bg-white p-2" v-html="qr" />
          <p class="mono text-center text-[28px] font-semibold tracking-wide">{{ code }}</p>
          <p class="text-center text-[12px] text-[var(--color-muted)]">Waiting for the other device…</p>
        </template>
        <button
          type="button"
          class="self-start rounded-md border border-[var(--color-line)] px-2.5 py-1"
          @click="cancelCode(); mode = 'choose'"
        >
          Back
        </button>
      </template>

      <template v-else-if="mode === 'enter'">
        <label class="block text-[12px] text-[var(--color-muted)]" for="join-code">Pairing code</label>
        <input
          id="join-code"
          v-model="joinCode"
          class="mono -mt-3 w-full rounded-md border border-[var(--color-line)] bg-[var(--color-canvas)] px-3 py-2"
          placeholder="12-3456-7890"
          autocomplete="off"
        />
        <label class="block text-[12px] text-[var(--color-muted)]" for="join-addr">
          Address, over Tailscale or a VPN (leave blank on the same network)
        </label>
        <input
          id="join-addr"
          v-model="joinAddr"
          class="-mt-3 w-full rounded-md border border-[var(--color-line)] bg-[var(--color-canvas)] px-3 py-2"
          placeholder="my-mac:47321 or 100.x.y.z:47321"
        />
        <div class="flex gap-2">
          <button type="button" class="rounded-md px-2.5 py-1" @click="mode = 'choose'">Back</button>
          <button
            type="button"
            class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)] disabled:opacity-60"
            :disabled="busy || !joinCode.trim()"
            @click="joinWithCode"
          >
            Pair
          </button>
        </div>
      </template>

      <template v-else>
        <p class="text-[var(--color-muted)]">
          Both devices need each other's id. Find this device's id in Settings.
        </p>
        <label class="block text-[12px] text-[var(--color-muted)]" for="peer-name">Name</label>
        <input
          id="peer-name"
          v-model="name"
          class="-mt-3 w-full rounded-md border border-[var(--color-line)] bg-[var(--color-canvas)] px-3 py-2"
          placeholder="Desktop"
        />
        <label class="block text-[12px] text-[var(--color-muted)]" for="peer-id">Device id</label>
        <input
          id="peer-id"
          v-model="deviceId"
          class="mono -mt-3 w-full rounded-md border border-[var(--color-line)] bg-[var(--color-canvas)] px-3 py-2"
          placeholder="64-character hex id from the other device"
        />
        <label class="block text-[12px] text-[var(--color-muted)]" for="peer-addr">Address</label>
        <input
          id="peer-addr"
          v-model="address"
          class="-mt-3 w-full rounded-md border border-[var(--color-line)] bg-[var(--color-canvas)] px-3 py-2"
          placeholder="192.168.1.20:47321"
        />
        <div class="flex gap-2">
          <button type="button" class="rounded-md px-2.5 py-1" @click="mode = 'choose'">Back</button>
          <button
            type="button"
            class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)] disabled:opacity-60"
            :disabled="busy || !name.trim() || !deviceId.trim()"
            @click="addManually"
          >
            Add
          </button>
        </div>
      </template>
    </div>
  </Sheet>
</template>
