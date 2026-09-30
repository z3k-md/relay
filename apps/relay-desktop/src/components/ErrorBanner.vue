<script setup lang="ts">
defineProps<{
  message: string | null;
}>();
</script>

<template>
  <div
    v-if="message"
    class="mb-3 rounded-md border border-red-300/60 bg-red-50 px-3 py-2 text-red-800 dark:border-red-900 dark:bg-red-950/60 dark:text-red-100"
  >
    {{ message }}
  </div>
</template>
