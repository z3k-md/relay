<script setup lang="ts">
import type { Tone } from "../lib/home";

defineProps<{ tone: Tone }>();
</script>

<template>
  <span
    class="inline-block size-2 shrink-0 rounded-full"
    :class="{
      'bg-[var(--color-ok)]': tone === 'ok',
      'bg-[var(--color-accent)]': tone === 'sync',
      'bg-[var(--color-warn)]': tone === 'warn',
      'bg-[var(--color-muted)] opacity-50': tone === 'off',
    }"
    aria-hidden="true"
  />
</template>
