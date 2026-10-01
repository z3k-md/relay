<script setup lang="ts">
import { ref } from "vue";
import ErrorBanner from "../components/ErrorBanner.vue";

const props = defineProps<{
  suggestedName: string;
}>();

const emit = defineEmits<{
  create: [name: string];
}>();

const name = ref(props.suggestedName);
const busy = ref(false);
const error = ref<string | null>(null);

async function submit() {
  const value = name.value.trim();
  if (!value) {
    error.value = "Give this computer a short name, like MacBook or Desktop.";
    return;
  }
  busy.value = true;
  error.value = null;
  try {
    emit("create", value);
  } finally {
    busy.value = false;
  }
}
</script>

<template>
  <div class="flex min-h-full items-center justify-center p-6">
    <div class="w-full max-w-md rounded-2xl border border-[var(--color-line)] bg-[var(--color-panel)] p-6 shadow-sm">
      <p class="text-[11px] font-semibold uppercase tracking-[0.14em] text-[var(--color-accent)]">
        First run
      </p>
      <h1 class="mt-1 text-xl font-semibold tracking-tight">Set up this device</h1>
      <p class="mt-2 text-[var(--color-muted)]">
        Relay keeps the folders you choose in sync across your computers, in real time.
        Name this machine to get started.
      </p>
      <ErrorBanner class="mt-3" :message="error" />
      <label class="mt-4 block text-[12px] font-medium text-[var(--color-muted)]" for="device-name">
        Device name
      </label>
      <input
        id="device-name"
        v-model="name"
        class="mt-1 w-full rounded-md border border-[var(--color-line)] bg-[var(--color-canvas)] px-3 py-2"
        maxlength="64"
        autocomplete="off"
        @keydown.enter="submit"
      />
      <button
        type="button"
        class="mt-4 w-full rounded-md bg-[var(--color-accent)] px-3 py-2 font-medium text-[var(--color-accent-fg)] disabled:opacity-60"
        :disabled="busy"
        @click="submit"
      >
        Create this device
      </button>
    </div>
  </div>
</template>
