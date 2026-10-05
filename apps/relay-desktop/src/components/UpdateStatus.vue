<script setup lang="ts">
import { computed } from "vue";
import { formatBytes } from "../lib/format";
import {
  restartNow,
  restartSeconds,
  updateNote,
  updateNoteIsError,
  updatePhase,
} from "../lib/updateProgress";

const phase = updatePhase;

const downloadLabel = computed(() => {
  if (phase.value.kind !== "downloading") return "";
  const got = formatBytes(phase.value.downloaded);
  if (phase.value.total == null || phase.value.total <= 0) return got;
  return `${got} / ${formatBytes(phase.value.total)}`;
});

const downloadPercent = computed(() => {
  if (phase.value.kind !== "downloading") return 0;
  const total = phase.value.total;
  if (total == null || total <= 0) return null;
  return Math.max(0, Math.min(100, (phase.value.downloaded / total) * 100));
});

async function restart() {
  try {
    await restartNow();
  } catch {
    // The process is already leaving; nothing else to show.
  }
}
</script>

<template>
  <div v-if="phase.kind !== 'idle' || updateNote" role="status" aria-live="polite">
    <div
      v-if="phase.kind === 'checking' || phase.kind === 'installing'"
      class="flex items-center gap-2 text-[12px] text-[var(--color-muted)]"
    >
      <span
        class="inline-block h-3.5 w-3.5 shrink-0 animate-spin rounded-full border-2 border-[var(--color-line)] border-t-[var(--color-accent)]"
        aria-hidden="true"
      />
      <span>{{ phase.kind === "checking" ? "Checking for updates…" : "Installing…" }}</span>
    </div>

    <div v-else-if="phase.kind === 'downloading'">
      <div class="mb-1 flex items-center justify-between gap-3 text-[12px] text-[var(--color-muted)]">
        <span>Downloading</span>
        <span class="tabular-nums">{{ downloadLabel }}</span>
      </div>
      <div
        class="h-1.5 overflow-hidden rounded-full bg-[var(--color-line)]"
        role="progressbar"
        :aria-valuemin="0"
        :aria-valuemax="downloadPercent == null ? undefined : 100"
        :aria-valuenow="downloadPercent == null ? undefined : Math.round(downloadPercent)"
        :aria-valuetext="downloadLabel"
      >
        <div
          v-if="downloadPercent == null"
          class="indet h-full rounded-full bg-[var(--color-accent)]"
        />
        <div
          v-else
          class="h-full rounded-full bg-[var(--color-accent)] transition-[width] duration-150"
          :style="{ width: `${downloadPercent}%` }"
        />
      </div>
    </div>

    <div v-else-if="phase.kind === 'ready'" class="flex flex-wrap items-center justify-between gap-2">
      <p class="text-[12px]">
        <template v-if="restartSeconds > 0">
          Relay {{ phase.version }} is ready. Restarting in {{ restartSeconds }}s.
        </template>
        <template v-else>Restarting…</template>
      </p>
      <button
        type="button"
        class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)]"
        @click="restart"
      >
        Restart now
      </button>
    </div>

    <p
      v-else-if="updateNote"
      class="text-[12px]"
      :class="updateNoteIsError ? 'text-[var(--color-danger)]' : 'text-[var(--color-muted)]'"
    >
      {{ updateNote }}
    </p>
  </div>
</template>

<style scoped>
.indet {
  width: 35%;
  animation: relay-indet 1.1s ease-in-out infinite;
}

@keyframes relay-indet {
  0% {
    transform: translateX(-120%);
  }
  100% {
    transform: translateX(320%);
  }
}
</style>
