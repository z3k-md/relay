<script setup lang="ts">
defineProps<{
  open: boolean;
  title: string;
}>();

const emit = defineEmits<{
  close: [];
}>();
</script>

<template>
  <Teleport to="body">
    <div
      v-if="open"
      class="fixed inset-0 z-50 flex cursor-pointer items-center justify-center bg-black/40 p-4"
      @click.self="emit('close')"
    >
      <div
        class="w-full max-w-md cursor-auto rounded-xl border border-[var(--color-line)] bg-[var(--color-panel)] p-4 shadow-xl"
        role="dialog"
        aria-modal="true"
      >
        <div class="mb-3 flex items-start justify-between gap-3">
          <h3 class="text-[15px] font-semibold">{{ title }}</h3>
          <button
            type="button"
            class="text-[var(--color-muted)] hover:text-[var(--color-ink)]"
            aria-label="Close"
            @click="emit('close')"
          >
            ✕
          </button>
        </div>
        <slot />
      </div>
    </div>
  </Teleport>
</template>
