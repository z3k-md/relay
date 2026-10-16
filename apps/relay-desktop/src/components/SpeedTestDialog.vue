<script setup lang="ts">
import { computed, onUnmounted, ref, watch } from "vue";
import Modal from "./Modal.vue";
import { api, errorText } from "../lib/api";
import { formatBytes } from "../lib/format";
import type { PathKind, SpeedReport } from "../lib/types";

const props = defineProps<{
  /** Peer to test, or null when closed. */
  peer: string | null;
}>();
const emit = defineEmits<{ close: [] }>();

/** Each direction runs this long on the host (SPEED_TEST_DEFAULT_MS). */
const LEG_MS = 5_000;
const report = ref<SpeedReport | null>(null);
const error = ref<string | null>(null);
const running = ref(false);
const elapsed = ref(0);
let timer: number | undefined;
let runId = 0;

const PATHS: Record<PathKind, string> = {
  loopback: "Same computer",
  lan: "Local network",
  tailscale: "Tailscale",
  internet: "Direct over the internet",
  relayed: "Through the relay",
};

function stopClock() {
  if (timer !== undefined) window.clearInterval(timer);
  timer = undefined;
}

async function run() {
  const peer = props.peer;
  if (!peer) return;
  const id = ++runId;
  report.value = null;
  error.value = null;
  running.value = true;
  const started = Date.now();
  elapsed.value = 0;
  stopClock();
  timer = window.setInterval(() => {
    elapsed.value = Date.now() - started;
  }, 100);
  try {
    const result = await api.speedTest(peer);
    if (id === runId) report.value = result;
  } catch (err) {
    if (id === runId) error.value = errorText(err);
  } finally {
    if (id === runId) {
      running.value = false;
      stopClock();
    }
  }
}

function close() {
  // A test still running finishes on the host; its answer is dropped.
  runId++;
  running.value = false;
  stopClock();
  emit("close");
}

watch(
  () => props.peer,
  (peer) => {
    if (peer) void run();
  },
);
onUnmounted(stopClock);

const phase = computed(() =>
  elapsed.value < LEG_MS ? "Measuring download…" : "Measuring upload…",
);
const progress = computed(() => Math.min(1, elapsed.value / (2 * LEG_MS)));

function mbps(bitsPerSec: number): string {
  const value = bitsPerSec / 1e6;
  return value >= 100 ? Math.round(value).toString() : value.toFixed(1);
}

const loss = computed(() => {
  const r = report.value;
  if (!r || r.sent_packets === 0) return "0%";
  return `${((r.lost_packets * 100) / r.sent_packets).toFixed(2)}%`;
});

/** Chart geometry: download then upload on one time axis. */
const W = 400;
const H = 120;
const chart = computed(() => {
  const r = report.value;
  if (!r) return null;
  const toMbps = (bytes: number) => (bytes * 8) / (r.sample_ms / 1000) / 1e6;
  const down = r.download.samples.map(toMbps);
  const up = r.upload.samples.map(toMbps);
  const slots = Math.max(1, down.length + up.length);
  const peak = Math.max(1, ...down, ...up);
  const top = niceCeil(peak);
  const x = (i: number) => (i / slots) * W;
  const y = (v: number) => H - (v / top) * H;
  const series = (values: number[], offset: number) => {
    if (values.length === 0) return { line: "", area: "" };
    const points = values.map((v, i) => `${x(offset + i + 0.5).toFixed(1)},${y(v).toFixed(1)}`);
    const first = x(offset + 0.5).toFixed(1);
    const last = x(offset + values.length - 0.5).toFixed(1);
    return {
      line: `M${points.join("L")}`,
      area: `M${first},${H}L${points.join("L")}L${last},${H}Z`,
    };
  };
  return {
    top,
    split: x(down.length),
    down: series(down, 0),
    up: series(up, down.length),
    seconds: Math.round((slots * r.sample_ms) / 1000),
  };
});

function niceCeil(v: number): number {
  const exp = 10 ** Math.floor(Math.log10(v));
  for (const step of [1, 2, 2.5, 5, 10]) {
    if (v <= step * exp) return step * exp;
  }
  return 10 * exp;
}
</script>

<template>
  <Modal :open="!!peer" :title="`Connection to ${peer ?? ''}`" @close="close">
    <div v-if="running">
      <p class="text-[13px]">{{ phase }}</p>
      <div class="mt-2 h-1.5 overflow-hidden rounded-full bg-[var(--color-line)]">
        <div
          class="h-full rounded-full bg-[var(--color-accent)] transition-[width] duration-100"
          :style="{ width: `${progress * 100}%` }"
        />
      </div>
      <p class="mt-2 text-[12px] text-[var(--color-muted)]">
        About ten seconds. Sync keeps running, so a busy link reads slower.
      </p>
    </div>
    <div v-else-if="error">
      <p class="text-[var(--color-danger)]">{{ error }}</p>
    </div>
    <div v-else-if="report">
      <div class="grid grid-cols-2 gap-3">
        <div>
          <p class="text-[12px] text-[var(--color-muted)]">
            <span class="mr-1 inline-block h-2 w-2 rounded-full bg-[var(--color-accent)]" />
            Download
          </p>
          <p class="text-[24px] font-semibold tabular-nums">
            {{ mbps(report.download.bits_per_sec) }}
            <span class="text-[13px] font-normal text-[var(--color-muted)]">Mbit/s</span>
          </p>
          <p class="text-[12px] text-[var(--color-muted)]">
            {{ formatBytes(report.download.bytes) }} from {{ peer }}
          </p>
        </div>
        <div>
          <p class="text-[12px] text-[var(--color-muted)]">
            <span class="mr-1 inline-block h-2 w-2 rounded-full bg-indigo-600 dark:bg-indigo-400" />
            Upload
          </p>
          <p class="text-[24px] font-semibold tabular-nums">
            {{ mbps(report.upload.bits_per_sec) }}
            <span class="text-[13px] font-normal text-[var(--color-muted)]">Mbit/s</span>
          </p>
          <p class="text-[12px] text-[var(--color-muted)]">
            {{ formatBytes(report.upload.bytes) }} to {{ peer }}
          </p>
        </div>
      </div>

      <figure v-if="chart" class="mt-3">
        <div class="flex justify-between text-[11px] text-[var(--color-muted)]">
          <span>{{ chart.top }} Mbit/s</span>
          <span>every {{ report.sample_ms }} ms</span>
        </div>
        <svg
          :viewBox="`0 0 ${W} ${H}`"
          preserveAspectRatio="none"
          class="block h-28 w-full"
          role="img"
          :aria-label="`Download ${mbps(report.download.bits_per_sec)} and upload ${mbps(report.upload.bits_per_sec)} megabits per second over time`"
        >
          <line
            :x1="0"
            :x2="W"
            :y1="H / 2"
            :y2="H / 2"
            stroke="var(--color-line)"
            stroke-dasharray="3 3"
            vector-effect="non-scaling-stroke"
          />
          <line
            :x1="chart.split"
            :x2="chart.split"
            :y1="0"
            :y2="H"
            stroke="var(--color-line)"
            vector-effect="non-scaling-stroke"
          />
          <g class="text-[var(--color-accent)]">
            <path :d="chart.down.area" fill="currentColor" fill-opacity="0.15" />
            <path
              :d="chart.down.line"
              fill="none"
              stroke="currentColor"
              stroke-width="2"
              stroke-linejoin="round"
              vector-effect="non-scaling-stroke"
            />
          </g>
          <g class="text-indigo-600 dark:text-indigo-400">
            <path :d="chart.up.area" fill="currentColor" fill-opacity="0.15" />
            <path
              :d="chart.up.line"
              fill="none"
              stroke="currentColor"
              stroke-width="2"
              stroke-linejoin="round"
              vector-effect="non-scaling-stroke"
            />
          </g>
          <line
            :x1="0"
            :x2="W"
            :y1="H"
            :y2="H"
            stroke="var(--color-muted)"
            vector-effect="non-scaling-stroke"
          />
        </svg>
        <div class="flex justify-between text-[11px] text-[var(--color-muted)]">
          <span>0 s</span>
          <span>{{ chart.seconds }} s</span>
        </div>
      </figure>

      <dl class="mt-3 grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 text-[12px]">
        <dt class="text-[var(--color-muted)]">Path</dt>
        <dd>{{ PATHS[report.path] }} <span class="mono text-[var(--color-muted)]">{{ report.address }}</span></dd>
        <dt class="text-[var(--color-muted)]">Round trip</dt>
        <dd class="tabular-nums">{{ (report.rtt_us / 1000).toFixed(1) }} ms</dd>
        <dt class="text-[var(--color-muted)]">Packet loss</dt>
        <dd class="tabular-nums">{{ loss }}</dd>
        <dt class="text-[var(--color-muted)]">Packet size</dt>
        <dd class="tabular-nums">{{ report.mtu }} bytes</dd>
      </dl>
    </div>
    <div class="mt-4 flex justify-end gap-2">
      <button type="button" class="rounded-md px-2.5 py-1" @click="close">Close</button>
      <button
        type="button"
        class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)] disabled:opacity-60"
        :disabled="running"
        @click="run"
      >
        {{ running ? "Testing…" : "Test again" }}
      </button>
    </div>
  </Modal>
</template>
