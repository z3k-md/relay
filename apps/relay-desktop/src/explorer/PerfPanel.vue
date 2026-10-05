<script setup lang="ts">
import { Channel } from "@tauri-apps/api/core";
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { type MemoryInfo, explorer } from "./api";
import { type ScrollResult, describeScroll, machineLines, runScroll } from "./bench";
import type { Folder } from "./folder";

const props = defineProps<{
  folder: Folder;
  tabs: number;
  native: boolean;
  /** Native drag and drop status line. */
  dnd: string;
  lastDrag: string;
  lastDrop: string;
  getScroller: () => HTMLElement | null;
}>();

const emit = defineEmits<{ navigate: [path: string] }>();

const MB = 1024 * 1024;
const stats = computed(() => props.folder.stats);

// --- live frame rate ----------------------------------------------------

const fps = ref(0);
const worstFrame = ref(0);
let raf = 0;
let stamps: number[] = [];
let lastShown = 0;

function tick(now: number) {
  stamps.push(now);
  while (stamps.length && stamps[0] < now - 1000) stamps.shift();
  // Update the panel twice a second, so it does not cost a render per frame.
  if (now - lastShown > 500) {
    lastShown = now;
    fps.value = stamps.length;
    let worst = 0;
    for (let i = 1; i < stamps.length; i++) worst = Math.max(worst, stamps[i] - stamps[i - 1]);
    worstFrame.value = worst;
  }
  raf = requestAnimationFrame(tick);
}

// --- scroll benchmark ---------------------------------------------------

const running = ref<string | null>(null);
const scrollResults = ref<ScrollResult[]>([]);

async function scroll(mode: "smooth" | "sweep") {
  const el = props.getScroller();
  if (!el || running.value) return;
  running.value = mode;
  const result = await runScroll(el, mode, props.folder.rows.value.length);
  scrollResults.value = [result, ...scrollResults.value.filter((r) => r.mode !== mode)];
  running.value = null;
}

// --- memory -------------------------------------------------------------

const memory = ref<MemoryInfo | null>(null);
const peak = ref(0);
let poll = 0;

async function readMemory() {
  try {
    memory.value = await explorer.memory();
    if (memory.value) peak.value = Math.max(peak.value, memory.value.workingSetBytes);
  } catch {
    memory.value = null;
  }
}

// --- benchmark folders --------------------------------------------------

const making = ref<string | null>(null);
const progress = ref(0);
const makeError = ref<string | null>(null);

async function makeBench(kind: "files" | "images", count: number) {
  if (making.value) return;
  making.value = `${kind}-${count}`;
  progress.value = 0;
  makeError.value = null;
  const channel = new Channel<number>();
  channel.onmessage = (n) => (progress.value = n);
  try {
    emit("navigate", await explorer.makeBench(kind, count, channel));
  } catch (err) {
    makeError.value = String(err);
  } finally {
    making.value = null;
  }
}

// --- pass bars ----------------------------------------------------------

const sweep = computed(() => scrollResults.value.find((r) => r.mode === "sweep"));

const bars = computed(() => [
  {
    label: "First paint ≤ 150 ms (10k folder)",
    value: stats.value.firstPaintMs === null ? "—" : `${stats.value.firstPaintMs.toFixed(0)} ms`,
    pass:
      stats.value.total >= 10_000 && stats.value.firstPaintMs !== null
        ? stats.value.firstPaintMs <= 150
        : null,
  },
  {
    label: "60 fps sweep (200k items)",
    value: sweep.value ? `${sweep.value.avgFps.toFixed(1)} fps, p95 ${sweep.value.p95.toFixed(1)} ms` : "—",
    pass:
      sweep.value && sweep.value.rows >= 200_000
        ? sweep.value.avgFps >= 57 && sweep.value.p95 <= 20 && sweep.value.blank <= sweep.value.frames / 100
        : null,
  },
  {
    label: "RSS ≤ 250 MB with 3 tabs",
    value: memory.value ? `${(memory.value.workingSetBytes / MB).toFixed(0)} MB, ${props.tabs} tabs` : "—",
    pass: memory.value && props.tabs >= 3 ? memory.value.workingSetBytes <= 250 * MB : null,
  },
]);

function mark(pass: boolean | null): string {
  return pass === null ? "·" : pass ? "✓" : "✗";
}

const copied = ref(false);

async function copyResults() {
  const s = stats.value;
  const lines = [
    `Relay Explorer P0 results (${new Date().toISOString()})`,
    ...machineLines(),
    `Native shell: ${props.native}; drag and drop: ${props.dnd}`,
    "",
    `Listing ${s.path}`,
    `  items ${s.total}, batches ${s.batches}`,
    `  first batch ${fmt(s.firstBatchMs)} ms, first paint ${fmt(s.firstPaintMs)} ms, all ${fmt(s.doneMs)} ms (Rust ${fmt(s.backendMs)} ms)`,
    `  sort/merge ${s.sortMs.toFixed(1)} ms, live changes ${s.changes}`,
    "",
    ...scrollResults.value.map((r) => `Scroll ${describeScroll(r)}`),
    memory.value
      ? `Memory: working set ${(memory.value.workingSetBytes / MB).toFixed(0)} MB (peak ${(peak.value / MB).toFixed(0)} MB), private ${(memory.value.privateBytes / MB).toFixed(0)} MB, ${memory.value.processes} processes, ${props.tabs} tabs`
      : "Memory: n/a",
    `Last drag: ${props.lastDrag || "none"}`,
    `Last drop: ${props.lastDrop || "none"}`,
    "",
    ...bars.value.map((b) => `${mark(b.pass)} ${b.label}: ${b.value}`),
  ];
  await navigator.clipboard.writeText(lines.join("\n"));
  copied.value = true;
  setTimeout(() => (copied.value = false), 1500);
}

function fmt(v: number | null): string {
  return v === null ? "—" : v.toFixed(0);
}

onMounted(() => {
  raf = requestAnimationFrame(tick);
  void readMemory();
  poll = window.setInterval(readMemory, 2000);
});

onBeforeUnmount(() => {
  cancelAnimationFrame(raf);
  clearInterval(poll);
  stamps = [];
});
</script>

<template>
  <aside class="flex w-80 shrink-0 flex-col gap-4 overflow-auto border-l border-line bg-panel p-3 text-xs">
    <section>
      <h2 class="mb-1 font-semibold">Pass bars</h2>
      <div v-for="bar in bars" :key="bar.label" class="flex gap-2 py-0.5">
        <span
          class="w-3 shrink-0 text-center"
          :class="bar.pass === null ? 'text-muted' : bar.pass ? 'text-ok' : 'text-danger'"
        >{{ mark(bar.pass) }}</span>
        <span class="flex-1">{{ bar.label }}</span>
        <span class="text-muted tabular-nums">{{ bar.value }}</span>
      </div>
    </section>

    <section>
      <h2 class="mb-1 font-semibold">Listing</h2>
      <dl class="grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 tabular-nums">
        <dt class="text-muted">Items</dt>
        <dd>{{ stats.total.toLocaleString() }} in {{ stats.batches }} batches</dd>
        <dt class="text-muted">First batch</dt>
        <dd>{{ fmt(stats.firstBatchMs) }} ms</dd>
        <dt class="text-muted">First paint</dt>
        <dd>{{ fmt(stats.firstPaintMs) }} ms</dd>
        <dt class="text-muted">All loaded</dt>
        <dd>{{ fmt(stats.doneMs) }} ms (Rust {{ fmt(stats.backendMs) }} ms)</dd>
        <dt class="text-muted">Sort/merge</dt>
        <dd>{{ stats.sortMs.toFixed(1) }} ms</dd>
        <dt class="text-muted">Live changes</dt>
        <dd>{{ stats.changes }}</dd>
      </dl>
    </section>

    <section>
      <h2 class="mb-1 font-semibold">Frames</h2>
      <p class="tabular-nums">{{ fps }} fps now, worst frame {{ worstFrame.toFixed(1) }} ms</p>
      <div class="mt-2 flex gap-2">
        <button
          class="rounded border border-line px-2 py-1 hover:bg-canvas disabled:opacity-50"
          :disabled="running !== null"
          @click="scroll('sweep')"
        >
          {{ running === "sweep" ? "Sweeping…" : "Sweep (5 s)" }}
        </button>
        <button
          class="rounded border border-line px-2 py-1 hover:bg-canvas disabled:opacity-50"
          :disabled="running !== null"
          @click="scroll('smooth')"
        >
          {{ running === "smooth" ? "Scrolling…" : "Smooth (10 s)" }}
        </button>
      </div>
      <div v-for="r in scrollResults" :key="r.mode" class="mt-2 tabular-nums">
        <span class="font-medium">{{ r.mode }}</span>
        {{ r.rows.toLocaleString() }} rows · {{ r.avgFps.toFixed(1) }} fps · p95
        {{ r.p95.toFixed(1) }} ms · p99 {{ r.p99.toFixed(1) }} ms · dropped {{ r.dropped }}/{{ r.frames }} · blank {{ r.blank }}
      </div>
    </section>

    <section>
      <h2 class="mb-1 font-semibold">Memory (app + WebView2)</h2>
      <p v-if="memory" class="tabular-nums">
        Working set {{ (memory.workingSetBytes / MB).toFixed(0) }} MB (peak {{ (peak / MB).toFixed(0) }} MB)<br />
        Private {{ (memory.privateBytes / MB).toFixed(0) }} MB · {{ memory.processes }} processes ·
        {{ tabs }} tabs
      </p>
      <p v-else class="text-muted">Windows only.</p>
    </section>

    <section>
      <h2 class="mb-1 font-semibold">Benchmark folders</h2>
      <div class="flex flex-wrap gap-2">
        <button
          class="rounded border border-line px-2 py-1 hover:bg-canvas disabled:opacity-50"
          :disabled="making !== null"
          @click="makeBench('files', 10_000)"
        >
          10k files
        </button>
        <button
          class="rounded border border-line px-2 py-1 hover:bg-canvas disabled:opacity-50"
          :disabled="making !== null"
          @click="makeBench('files', 200_000)"
        >
          200k files
        </button>
        <button
          class="rounded border border-line px-2 py-1 hover:bg-canvas disabled:opacity-50"
          :disabled="making !== null"
          @click="makeBench('images', 2_000)"
        >
          2k images
        </button>
      </div>
      <p v-if="making" class="mt-1 text-muted tabular-nums">
        Creating {{ making }}: {{ progress.toLocaleString() }}…
      </p>
      <p v-if="makeError" class="mt-1 text-danger">{{ makeError }}</p>
      <p class="mt-1 text-muted">Created once in the temp folder, then reused.</p>
    </section>

    <section>
      <h2 class="mb-1 font-semibold">Drag and drop</h2>
      <p>{{ dnd }}</p>
      <p v-if="lastDrag" class="mt-1 break-words text-muted">{{ lastDrag }}</p>
      <p v-if="lastDrop" class="mt-1 break-words">{{ lastDrop }}</p>
    </section>

    <button
      class="rounded bg-accent px-3 py-1.5 text-accent-fg hover:opacity-90"
      @click="copyResults"
    >
      {{ copied ? "Copied" : "Copy results" }}
    </button>
  </aside>
</template>
