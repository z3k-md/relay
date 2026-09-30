<script setup lang="ts">
import { computed } from "vue";
import { runnerLabel } from "../lib/api";
import type { RunnerState } from "../lib/types";

const props = defineProps<{
  state: RunnerState;
}>();

const tone = computed(() => {
  switch (props.state.kind) {
    case "running":
      return "bg-emerald-100 text-emerald-800 dark:bg-emerald-950 dark:text-emerald-200";
    case "paused":
      return "bg-amber-100 text-amber-800 dark:bg-amber-950 dark:text-amber-200";
    case "starting":
      return "bg-sky-100 text-sky-800 dark:bg-sky-950 dark:text-sky-200";
    case "error":
      return "bg-red-100 text-red-800 dark:bg-red-950 dark:text-red-200";
    case "externalService":
      return "bg-indigo-100 text-indigo-800 dark:bg-indigo-950 dark:text-indigo-200";
    default:
      return "bg-zinc-100 text-zinc-700 dark:bg-zinc-800 dark:text-zinc-200";
  }
});
</script>

<template>
  <span class="inline-flex items-center rounded-full px-2 py-0.5 text-[12px] font-medium" :class="tone">
    {{ runnerLabel(state) }}
  </span>
</template>
