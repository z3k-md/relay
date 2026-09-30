import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { computed, ref } from "vue";
import { api } from "./api";
import type { UpdateInfo, UpdateProgress } from "./types";

export type UpdatePhase =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "downloading"; downloaded: number; total: number | null }
  | { kind: "installing" }
  | { kind: "ready"; version: string; restartAtMs: number };

export const updatePhase = ref<UpdatePhase>({ kind: "idle" });
export const updateNote = ref<string | null>(null);
export const updateNoteIsError = ref(false);

const nowMs = ref(Date.now());

let clock: number | null = null;
let unlisten: UnlistenFn | null = null;
let appliedOp = 0;
let opTerminal = false;

export const updateActive = computed(() => updatePhase.value.kind !== "idle");

export const restartSeconds = computed(() => {
  if (updatePhase.value.kind !== "ready") return 0;
  const left = updatePhase.value.restartAtMs - nowMs.value;
  return Math.max(0, Math.ceil(left / 1000));
});

export async function listenForUpdateProgress(): Promise<void> {
  if (unlisten) return;
  unlisten = await listen<UpdateProgress>("relay://update-progress", (event) => {
    applyProgress(event.payload);
  });
}

export function stopUpdateProgressListener(): void {
  unlisten?.();
  unlisten = null;
  stopClock();
}

export async function checkForUpdates(): Promise<void> {
  beginLocal();
  try {
    finish(await api.checkForUpdates());
  } catch (err) {
    settleLocalError(err);
  }
}

export async function installUpdate(): Promise<void> {
  beginLocal();
  try {
    finish(await api.installUpdate());
  } catch (err) {
    settleLocalError(err);
  }
}

export async function restartNow(): Promise<void> {
  await api.restartApp();
}

export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "0 B";
  if (bytes < 1024) return `${Math.round(bytes)} B`;
  const kb = bytes / 1024;
  if (kb < 1024) return `${kb < 10 ? kb.toFixed(1) : Math.round(kb)} KB`;
  const mb = kb / 1024;
  if (mb < 1024) return `${mb < 10 ? mb.toFixed(1) : Math.round(mb)} MB`;
  const gb = mb / 1024;
  return `${gb < 10 ? gb.toFixed(1) : Math.round(gb)} GB`;
}

function beginLocal() {
  updateNote.value = null;
  updateNoteIsError.value = false;
  updatePhase.value = { kind: "checking" };
}

function applyProgress(progress: UpdateProgress) {
  if (!acceptOp(progress.opId, isTerminal(progress.kind))) return;
  switch (progress.kind) {
    case "checking":
      updateNote.value = null;
      updateNoteIsError.value = false;
      updatePhase.value = { kind: "checking" };
      break;
    case "downloading":
      updatePhase.value = {
        kind: "downloading",
        downloaded: progress.downloaded,
        total: progress.total,
      };
      break;
    case "installing":
      updatePhase.value = { kind: "installing" };
      break;
    case "ready":
      updateNote.value = null;
      updateNoteIsError.value = false;
      updatePhase.value = {
        kind: "ready",
        version: progress.version,
        restartAtMs: progress.restartAtMs,
      };
      startClock();
      break;
    case "finished":
      updatePhase.value = { kind: "idle" };
      updateNote.value = progress.message;
      updateNoteIsError.value = progress.error;
      stopClock();
      break;
  }
}

function finish(info: UpdateInfo) {
  if (info.opId !== 0 && !acceptOp(info.opId, true)) return;
  if (info.opId === 0) opTerminal = true;
  if (info.restartAtMs != null) {
    updateNote.value = null;
    updateNoteIsError.value = false;
    updatePhase.value = {
      kind: "ready",
      version: info.version ?? "",
      restartAtMs: info.restartAtMs,
    };
    startClock();
    return;
  }
  updatePhase.value = { kind: "idle" };
  updateNote.value = info.message;
  updateNoteIsError.value = info.error;
  stopClock();
}

function settleLocalError(err: unknown) {
  opTerminal = true;
  updatePhase.value = { kind: "idle" };
  updateNote.value = err instanceof Error ? err.message : String(err);
  updateNoteIsError.value = true;
  stopClock();
}

function acceptOp(opId: number, terminal: boolean): boolean {
  if (opId < appliedOp) return false;
  if (opId === appliedOp && opTerminal && !terminal) return false;
  if (opId > appliedOp) {
    appliedOp = opId;
    opTerminal = false;
  }
  if (terminal) opTerminal = true;
  return true;
}

function isTerminal(kind: UpdateProgress["kind"]): boolean {
  return kind === "ready" || kind === "finished";
}

function startClock() {
  nowMs.value = Date.now();
  if (clock != null) return;
  clock = window.setInterval(() => {
    nowMs.value = Date.now();
    if (updatePhase.value.kind !== "ready") stopClock();
  }, 200);
}

function stopClock() {
  if (clock != null) {
    window.clearInterval(clock);
    clock = null;
  }
}
