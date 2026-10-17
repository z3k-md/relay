import { Channel } from "@tauri-apps/api/core";
import { reactive, ref, shallowRef } from "vue";
import { type Change, type Entry, FLAG_DIR, FLAG_HIDDEN, type ListEvent, explorer } from "./api";

export type SortKey = "name" | "modified" | "type" | "size";
export type ViewMode = "details" | "grid";

/** Timings for the current listing, for the perf panel. */
export interface ListingStats {
  path: string;
  /** invoke → first batch received, ms. */
  firstBatchMs: number | null;
  /** invoke → first rows painted, ms. */
  firstPaintMs: number | null;
  /** invoke → last batch applied, ms. */
  doneMs: number | null;
  /** Enumeration time measured in Rust, ms. */
  backendMs: number | null;
  batches: number;
  total: number;
  /** Time spent sorting and merging on the UI thread, ms. */
  sortMs: number;
  changes: number;
}

// Explorer orders names with StrCmpLogicalW: case-insensitive, digit runs
// by value. A numeric collator is the closest web equivalent.
const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });

function typeOf(entry: Entry): string {
  if (entry[1] & FLAG_DIR) return "";
  const dot = entry[0].lastIndexOf(".");
  return dot > 0 ? entry[0].slice(dot + 1).toLowerCase() : "";
}

export function comparator(key: SortKey, desc: boolean): (a: Entry, b: Entry) => number {
  const dir = desc ? -1 : 1;
  const byName = (a: Entry, b: Entry) => collator.compare(a[0], b[0]);
  const primary: (a: Entry, b: Entry) => number =
    key === "name"
      ? byName
      : key === "modified"
        ? (a, b) => a[3] - b[3]
        : key === "size"
          ? (a, b) => a[2] - b[2]
          : (a, b) => collator.compare(typeOf(a), typeOf(b));
  return (a, b) => {
    // Folders first, whatever the order, as Explorer does.
    const folders = (b[1] & FLAG_DIR) - (a[1] & FLAG_DIR);
    if (folders !== 0) return folders;
    return dir * (primary(a, b) || byName(a, b));
  };
}

function merge(a: Entry[], b: Entry[], cmp: (a: Entry, b: Entry) => number): Entry[] {
  if (a.length === 0) return b;
  if (b.length === 0) return a;
  const out: Entry[] = new Array(a.length + b.length);
  let i = 0;
  let j = 0;
  let k = 0;
  while (i < a.length && j < b.length) out[k++] = cmp(a[i], b[j]) <= 0 ? a[i++] : b[j++];
  while (i < a.length) out[k++] = a[i++];
  while (j < b.length) out[k++] = b[j++];
  return out;
}

function insertSorted(rows: Entry[], entry: Entry, cmp: (a: Entry, b: Entry) => number) {
  let lo = 0;
  let hi = rows.length;
  while (lo < hi) {
    const mid = (lo + hi) >>> 1;
    if (cmp(rows[mid], entry) <= 0) lo = mid + 1;
    else hi = mid;
  }
  rows.splice(lo, 0, entry);
}

let nextTabId = 1;

/**
 * One tab: a folder listing that streams in, stays live, and keeps its own
 * history, selection and sort. Rows live in a shallow ref holding a plain
 * array, so Vue never walks 200k entries.
 */
export function createFolder(initial: string) {
  const id = `t${nextTabId++}`;
  const path = ref(initial);
  const back = ref<string[]>([]);
  const forward = ref<string[]>([]);
  const all = new Map<string, Entry>();
  const rows = shallowRef<Entry[]>([]);
  const selected = shallowRef<Set<string>>(new Set());
  const anchor = ref<number | null>(null);
  const loading = ref(false);
  const error = ref<string | null>(null);
  const view = ref<ViewMode>("details");
  const sortKey = ref<SortKey>("name");
  const sortDesc = ref(false);
  const showHidden = ref(false);
  const stats = reactive<ListingStats>(emptyStats(initial));
  /** Where the view was scrolled, restored when the tab is shown again. */
  let scrollTop = 0;

  let generation = 0;
  let pendingBatch: Entry[] = [];
  let pendingChanges: Change[] = [];
  let frame = 0;
  let started = 0;
  let streaming = false;
  /** "done" arrived; time it once the last batch is on screen. */
  let donePending = false;
  /** Reloads forced by the watcher losing events, for diagnostics. */
  let rescans = 0;

  const visible = (e: Entry) => showHidden.value || !(e[1] & FLAG_HIDDEN);
  const cmp = () => comparator(sortKey.value, sortDesc.value);

  function emptyStats(p: string): ListingStats {
    return {
      path: p,
      firstBatchMs: null,
      firstPaintMs: null,
      doneMs: null,
      backendMs: null,
      batches: 0,
      total: 0,
      sortMs: 0,
      changes: 0,
    };
  }

  /** Run `f` after the next frame is painted, unless the tab moved on. */
  function afterPaint(f: () => void) {
    const gen = generation;
    requestAnimationFrame(() =>
      setTimeout(() => {
        if (gen === generation) f();
      }),
    );
  }

  function schedule() {
    if (!frame) frame = requestAnimationFrame(flush);
  }

  /** Apply queued batches and changes once per frame. */
  function flush() {
    frame = 0;
    const t0 = performance.now();
    const order = cmp();
    let next = rows.value;
    if (pendingBatch.length) {
      const batch = pendingBatch.filter(visible).sort(order);
      pendingBatch = [];
      next = merge(next, batch, order);
    }
    if (pendingChanges.length && !streaming) {
      next = applyChanges(next, pendingChanges, order);
      pendingChanges = [];
    }
    stats.sortMs += performance.now() - t0;
    rows.value = next;
    if (stats.firstPaintMs === null && next.length > 0) {
      afterPaint(() => {
        if (stats.firstPaintMs === null) stats.firstPaintMs = performance.now() - started;
      });
    }
    if (donePending) {
      donePending = false;
      afterPaint(() => (stats.doneMs = performance.now() - started));
    }
  }

  function applyChanges(current: Entry[], changes: Change[], order: (a: Entry, b: Entry) => number) {
    const removed = new Set<string>();
    const added: Entry[] = [];
    for (const change of changes) {
      stats.changes++;
      switch (change.kind) {
        case "rescan":
          rescans++;
          void load(path.value, false);
          return current;
        case "removed":
          all.delete(change.name);
          removed.add(change.name);
          break;
        case "renamed":
          all.delete(change.from);
          removed.add(change.from);
          all.set(change.entry[0], change.entry);
          removed.add(change.entry[0]);
          added.push(change.entry);
          break;
        case "upsert":
          all.set(change.entry[0], change.entry);
          removed.add(change.entry[0]);
          added.push(change.entry);
          break;
      }
    }
    // Last write wins for names that changed twice in one window.
    const latest = new Map(added.map((e) => [e[0], e]));
    const fresh = [...latest.values()].filter((e) => all.get(e[0]) === e && visible(e));
    let next = removed.size ? current.filter((e) => !removed.has(e[0])) : current.slice();
    if (fresh.length < 32) {
      for (const e of fresh) insertSorted(next, e, order);
    } else {
      next = merge(next, fresh.sort(order), order);
    }
    if (selected.value.size && [...removed].some((n) => selected.value.has(n) && !all.has(n))) {
      selected.value = new Set([...selected.value].filter((n) => all.has(n)));
    }
    stats.total = all.size;
    return next;
  }

  function onEvent(gen: number, event: ListEvent) {
    if (gen !== generation) return;
    switch (event.kind) {
      case "batch":
        if (stats.firstBatchMs === null) stats.firstBatchMs = performance.now() - started;
        stats.batches++;
        // No push(...entries): a batch can hold 200k entries, past the
        // engine's argument limit.
        for (const e of event.entries) {
          all.set(e[0], e);
          pendingBatch.push(e);
        }
        stats.total = all.size;
        schedule();
        break;
      case "done":
        streaming = false;
        loading.value = false;
        stats.backendMs = event.elapsedMs;
        donePending = true;
        // Also applies changes that queued behind the listing.
        schedule();
        break;
      case "error":
        streaming = false;
        loading.value = false;
        error.value = event.message;
        break;
      case "changes":
        for (const c of event.changes) pendingChanges.push(c);
        schedule();
        break;
    }
  }

  async function load(target: string, resetSelection = true) {
    generation++;
    const gen = generation;
    path.value = target;
    all.clear();
    rows.value = [];
    pendingBatch = [];
    pendingChanges = [];
    donePending = false;
    if (resetSelection) {
      selected.value = new Set();
      anchor.value = null;
      scrollTop = 0;
    }
    error.value = null;
    loading.value = true;
    streaming = true;
    Object.assign(stats, emptyStats(target));
    started = performance.now();
    const channel = new Channel<ListEvent>();
    channel.onmessage = (event) => {
      // A handler that throws stalls the channel for good (later messages
      // wait on this one), so surface the failure instead.
      try {
        onEvent(gen, event);
      } catch (err) {
        console.error("listing event", err);
        if (gen === generation) {
          streaming = false;
          loading.value = false;
          error.value = String(err);
        }
      }
    };
    try {
      await explorer.list(id, target, channel);
    } catch (err) {
      if (gen === generation) {
        loading.value = false;
        streaming = false;
        error.value = String(err);
      }
    }
  }

  function navigate(target: string) {
    if (target === path.value) return void load(target);
    back.value.push(path.value);
    forward.value = [];
    void load(target);
  }

  function goBack() {
    const target = back.value.pop();
    if (target === undefined) return;
    forward.value.push(path.value);
    void load(target);
  }

  function goForward() {
    const target = forward.value.pop();
    if (target === undefined) return;
    back.value.push(path.value);
    void load(target);
  }

  /** Re-sort or re-filter everything we have, e.g. after a header click. */
  function resort() {
    const t0 = performance.now();
    rows.value = [...all.values()].filter(visible).sort(cmp());
    stats.sortMs = performance.now() - t0;
  }

  function sortBy(key: SortKey) {
    if (sortKey.value === key) sortDesc.value = !sortDesc.value;
    else {
      sortKey.value = key;
      sortDesc.value = false;
    }
    resort();
  }

  function setShowHidden(on: boolean) {
    showHidden.value = on;
    resort();
  }

  function close() {
    generation++;
    void explorer.closeTab(id);
  }

  return {
    id,
    path,
    rows,
    selected,
    anchor,
    loading,
    error,
    view,
    sortKey,
    sortDesc,
    showHidden,
    stats,
    canBack: () => back.value.length > 0,
    canForward: () => forward.value.length > 0,
    /** Entries received but not yet applied (applied once per frame). */
    get pending() {
      return pendingBatch.length;
    },
    get rescans() {
      return rescans;
    },
    get scrollTop() {
      return scrollTop;
    },
    set scrollTop(v: number) {
      scrollTop = v;
    },
    load,
    navigate,
    goBack,
    goForward,
    sortBy,
    setShowHidden,
    close,
  };
}

export type Folder = ReturnType<typeof createFolder>;
