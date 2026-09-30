<script setup lang="ts">
defineProps<{
  title: string;
  body: string;
}>();
</script>

<template>
  <div class="rounded-lg border border-dashed border-[var(--color-line)] bg-[var(--color-panel)] px-5 py-8 text-center">
    <p class="font-medium">{{ title }}</p>
    <p class="mt-1 text-[var(--color-muted)]">{{ body }}</p>
    <div v-if="$slots.default" class="mt-4">
      <slot />
    </div>
  </div>
</template>
