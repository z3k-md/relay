<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import EmptyState from "../components/EmptyState.vue";
import ErrorBanner from "../components/ErrorBanner.vue";
import Modal from "../components/Modal.vue";
import { api } from "../lib/api";
import type { ConflictView } from "../lib/types";

const items = ref<ConflictView[]>([]);
const loading = ref(true);
const error = ref<string | null>(null);
const busy = ref(false);
const confirm = ref<
  | { kind: "file"; item: ConflictView; keep: "current" | "copy" }
  | { kind: "git"; space: string; mount: string; gitDir: string; branches: boolean }
  | null
>(null);

const files = computed(() => items.value.filter((item) => item.class.kind === "file"));

interface GitGroup {
  space: string;
  mount: string;
  gitDir: string;
  branches: ConflictView[];
  metadata: ConflictView[];
  from: string;
}

const gitGroups = computed(() => {
  const map = new Map<string, GitGroup>();
  for (const item of items.value) {
    if (item.class.kind !== "git") continue;
    const key = `${item.space}/${item.mount}/${item.class.gitDir}`;
    let group = map.get(key);
    if (!group) {
      group = {
        space: item.space,
        mount: item.mount,
        gitDir: item.class.gitDir,
        branches: [],
        metadata: [],
        from: "",
      };
      map.set(key, group);
    }
    if (item.class.isRef) group.branches.push(item);
    else group.metadata.push(item);
  }
  for (const group of map.values()) {
    const names = new Set<string>();
    for (const copy of [...group.branches, ...group.metadata]) {
      names.add(copy.deviceName ?? copy.deviceShort);
    }
    group.from = [...names].join(", ");
  }
  return [...map.values()];
});

async function load() {
  loading.value = true;
  error.value = null;
  try {
    items.value = await api.listConflicts();
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  } finally {
    loading.value = false;
  }
}

function confirmTitle(): string {
  if (!confirm.value) return "";
  if (confirm.value.kind === "file") {
    return confirm.value.keep === "current" ? "Keep current version" : "Use this version";
  }
  return confirm.value.branches ? "Delete branches too" : "Clean up metadata copies";
}

async function runConfirmed() {
  if (!confirm.value) return;
  busy.value = true;
  error.value = null;
  const action = confirm.value;
  confirm.value = null;
  try {
    if (action.kind === "file") {
      await api.resolveConflict(action.item.space, action.item.mount, action.item.path, action.keep);
    } else {
      await api.resolveGitConflicts(action.space, action.mount, action.gitDir, action.branches);
    }
    await load();
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  } finally {
    busy.value = false;
  }
}

onMounted(load);
defineExpose({ load });
</script>

<template>
  <div>
    <h2 class="mb-3 text-[15px] font-semibold">Conflicts</h2>
    <ErrorBanner :message="error" />
    <p v-if="loading" class="text-[var(--color-muted)]">Loading conflicts…</p>
    <EmptyState
      v-else-if="items.length === 0"
      title="No conflicts"
      body="When two devices edit the same file at once, Relay keeps both copies. The losing version shows up here."
    />
    <div v-else class="space-y-3">
      <ul v-if="files.length" class="space-y-2">
        <li
          v-for="(item, i) in files"
          :key="`${item.space}/${item.mount}/${item.path}-${i}`"
          class="rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)] px-3 py-2"
        >
          <p class="mono break-all text-[13px]">{{ item.space }}/{{ item.mount }}/{{ item.path }}</p>
          <p class="text-[12px] text-[var(--color-muted)]">
            From {{ item.deviceName ?? "device" }}
            <span class="mono">({{ item.deviceShort }})</span>
          </p>
          <div class="mt-2 flex flex-wrap gap-2">
            <button
              type="button"
              class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)]"
              :disabled="busy"
              @click="confirm = { kind: 'file', item, keep: 'current' }"
            >
              Keep current
            </button>
            <button
              type="button"
              class="rounded-md border border-[var(--color-line)] px-2.5 py-1"
              :disabled="busy"
              @click="confirm = { kind: 'file', item, keep: 'copy' }"
            >
              Use this version
            </button>
          </div>
        </li>
      </ul>

      <ul v-if="gitGroups.length" class="space-y-2">
        <li
          v-for="group in gitGroups"
          :key="`${group.space}/${group.mount}/${group.gitDir}`"
          class="rounded-lg border border-[var(--color-line)] bg-[var(--color-panel)] px-3 py-2"
        >
          <p class="font-medium">{{ group.space }}/{{ group.mount }}: {{ group.gitDir }}</p>
          <p class="text-[12px] text-[var(--color-muted)]">
            {{ group.branches.length }}
            {{ group.branches.length === 1 ? "branch" : "branches" }}
            <span v-if="group.branches.length">
              ({{ group.branches.map((b) => b.path).join(", ") }})
            </span>
            · {{ group.metadata.length }} metadata
            {{ group.metadata.length === 1 ? "copy" : "copies" }}
            <span v-if="group.from"> · from {{ group.from }}</span>
          </p>
          <p class="mt-1 text-[12px] text-[var(--color-muted)]">
            Conflicting branches stay so you can merge them in Git.
          </p>
          <div class="mt-2 flex flex-wrap gap-2">
            <button
              type="button"
              class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)]"
              :disabled="busy || group.metadata.length === 0"
              @click="
                confirm = {
                  kind: 'git',
                  space: group.space,
                  mount: group.mount,
                  gitDir: group.gitDir,
                  branches: false,
                }
              "
            >
              Clean up metadata copies
            </button>
            <button
              type="button"
              class="rounded-md border border-[var(--color-line)] px-2.5 py-1"
              :disabled="busy || group.branches.length === 0"
              @click="
                confirm = {
                  kind: 'git',
                  space: group.space,
                  mount: group.mount,
                  gitDir: group.gitDir,
                  branches: true,
                }
              "
            >
              Also delete branches
            </button>
          </div>
        </li>
      </ul>
    </div>

    <Modal :open="!!confirm" :title="confirmTitle()" @close="confirm = null">
      <p v-if="confirm?.kind === 'file' && confirm.keep === 'current'" class="mb-4 text-[13px]">
        Delete
        <span class="mono">{{ confirm.item.path }}</span>
        and keep the current file. Older versions stay in history.
      </p>
      <p v-else-if="confirm?.kind === 'file'" class="mb-4 text-[13px]">
        Replace the original with
        <span class="mono">{{ confirm.item.path }}</span>
        (the copy’s current bytes). Older versions stay in history.
      </p>
      <p v-else-if="confirm?.kind === 'git' && !confirm.branches" class="mb-4 text-[13px]">
        Delete metadata conflict copies under
        <span class="mono">{{ confirm.gitDir }}</span>.
        Conflicting branches remain so you can merge them in Git.
      </p>
      <p v-else class="mb-4 text-[13px]">
        Delete metadata copies and conflicting branches under
        <span class="mono">{{ confirm?.kind === "git" ? confirm.gitDir : "" }}</span>.
      </p>
      <div class="flex justify-end gap-2">
        <button type="button" class="rounded-md px-2.5 py-1" @click="confirm = null">Cancel</button>
        <button
          type="button"
          class="rounded-md bg-[var(--color-accent)] px-2.5 py-1 text-[var(--color-accent-fg)]"
          :disabled="busy"
          @click="runConfirmed"
        >
          Confirm
        </button>
      </div>
    </Modal>
  </div>
</template>
