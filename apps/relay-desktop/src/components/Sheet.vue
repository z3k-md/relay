<script setup lang="ts">
import { onMounted, onUnmounted, ref } from "vue";

defineProps<{
  title: string;
  subtitle?: string | null;
  /** Show a back arrow instead of the close button: another sheet is under this one. */
  back?: boolean;
  /** Wider panel for file lists. */
  wide?: boolean;
}>();

const emit = defineEmits<{
  close: [];
}>();

const root = ref<HTMLElement | null>(null);

function onKey(event: KeyboardEvent) {
  // Only the sheet on top closes: one under it is hidden, so it has no boxes.
  if (!root.value?.getClientRects().length) return;
  if (event.key === "Escape" && !document.querySelector("[role=dialog][aria-modal=true]")) {
    emit("close");
  }
}

onMounted(() => window.addEventListener("keydown", onKey));
onUnmounted(() => window.removeEventListener("keydown", onKey));
</script>

<template>
  <div ref="root" class="fixed inset-0 z-40 flex justify-end">
    <div class="absolute inset-0 hidden bg-black/30 min-[800px]:block" @click="emit('close')" />
    <section
      class="relative flex h-full w-full flex-col bg-[var(--color-panel)] shadow-2xl min-[800px]:border-l min-[800px]:border-[var(--color-line)]"
      :class="wide ? 'min-[800px]:w-[min(880px,92vw)]' : 'min-[800px]:w-[min(560px,92vw)]'"
      role="region"
      :aria-label="title"
    >
      <header class="flex items-start gap-3 border-b border-[var(--color-line)] px-4 py-3">
        <button
          v-if="back"
          type="button"
          class="-ml-1 rounded-md px-1.5 text-[18px] leading-6 text-[var(--color-muted)] hover:text-[var(--color-ink)]"
          aria-label="Back"
          @click="emit('close')"
        >
          ‹
        </button>
        <div class="min-w-0 flex-1">
          <h2 class="truncate text-[16px] font-semibold tracking-tight">{{ title }}</h2>
          <p v-if="subtitle" class="mono truncate text-[12px] text-[var(--color-muted)]" :title="subtitle">
            {{ subtitle }}
          </p>
        </div>
        <div class="flex shrink-0 items-center gap-2">
          <slot name="actions" />
          <button
            v-if="!back"
            type="button"
            class="rounded-md px-1.5 text-[var(--color-muted)] hover:text-[var(--color-ink)]"
            aria-label="Close"
            @click="emit('close')"
          >
            ✕
          </button>
        </div>
      </header>
      <div class="min-h-0 flex-1 overflow-auto px-4 py-4">
        <slot />
      </div>
    </section>
  </div>
</template>
