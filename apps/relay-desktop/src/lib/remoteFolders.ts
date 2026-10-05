import { computed, ref } from "vue";
import { api, RemoteCallError } from "./api";
import type { DirListing, FolderSize, RemoteRoot } from "./types";

/** Turn a refused remote call into a sentence about `device`. */
export function describeRemoteError(err: unknown, device: string): string {
  if (!(err instanceof RemoteCallError)) {
    return err instanceof Error ? err.message : String(err);
  }
  switch (err.code) {
    case "denied":
      return `${device} needs to allow this. ${err.message}`;
    case "forbidden":
      return `${device} has not allowed this computer to manage it. Turn it on in Peers on ${device}.`;
    case "offline":
      return `${device} is not connected.`;
    case "unsupported":
      return `${device} runs an older Relay. Update it to browse it.`;
    case "timeout":
      return `${device} did not answer in time. A permission prompt may be waiting on it.`;
    default:
      return err.message;
  }
}

/** Where browsing is: a folder path, or null for the device's roots. */
type Place = string | null;

/** How often folder sizes are asked for while they are being counted. */
const SIZE_POLL_MS = 700;

/**
 * Browsing state for one paired device: its roots, the folder shown, paging,
 * back/forward history, and folder sizes if `options.sizes`. `listing` null
 * means the roots are shown.
 *
 * Folders already seen show at once from a cache and are refreshed behind
 * the scenes, so going back and forth never waits on the network.
 */
export function useRemoteFolders(options: { sizes?: boolean } = {}) {
  const device = ref<string | null>(null);
  const roots = ref<RemoteRoot[]>([]);
  const listing = ref<DirListing | null>(null);
  const loading = ref(false);
  const error = ref<string | null>(null);
  /** A folder being opened that has no cached listing yet. */
  const pending = ref<string | null>(null);
  const history = ref<Place[]>([null]);
  const index = ref(0);
  /** Folder sizes for the folder shown, by entry path. */
  const sizes = ref<Map<string, FolderSize>>(new Map());
  /** All folders inside the folder shown are counted. */
  const sizesDone = ref(false);
  /** The device counts folder sizes (older ones do not). */
  const sizesSupported = ref(true);

  const listings = new Map<string, DirListing>();
  const rootsSeen = new Map<string, RemoteRoot[]>();
  const sizesSeen = new Map<string, { folders: Map<string, FolderSize>; done: boolean }>();
  /** Devices that answered `folder_sizes` with an error: older Relay. */
  const noSizes = new Set<string>();
  /** Bumped on every navigation; late answers for an older one are dropped. */
  let generation = 0;
  let sizeTimer: number | undefined;

  const canBack = computed(() => index.value > 0);
  const canForward = computed(() => index.value < history.value.length - 1);

  function key(name: string, path: string): string {
    return `${name}\u0000${path}`;
  }

  async function choose(name: string) {
    device.value = name;
    listing.value = null;
    pending.value = null;
    history.value = [null];
    index.value = 0;
    error.value = null;
    stopSizes();
    sizesSupported.value = !noSizes.has(name);
    roots.value = rootsSeen.get(name) ?? [];
    await loadRoots(name);
  }

  async function loadRoots(name: string) {
    const token = ++generation;
    loading.value = true;
    try {
      const reply = await api.remoteCall(name, { call: "roots" });
      if (token !== generation) return;
      if (reply.reply === "roots") {
        roots.value = reply.roots;
        rootsSeen.set(name, reply.roots);
      }
    } catch (err) {
      if (token === generation) error.value = describeRemoteError(err, name);
    } finally {
      if (token === generation) loading.value = false;
    }
  }

  /**
   * Show `place`, then record it with `step`, which gets the canonical path.
   * A cached listing shows at once and is refreshed; otherwise the current
   * folder stays up until the new one arrives, and stays if it fails.
   */
  async function go(place: Place, step: (path: Place) => void) {
    const name = device.value;
    if (!name) return;
    const token = ++generation;
    error.value = null;
    stopSizes();
    if (place === null) {
      listing.value = null;
      pending.value = null;
      loading.value = false;
      step(null);
      return;
    }
    const cached = listings.get(key(name, place));
    if (cached) {
      listing.value = cached;
      step(cached.path);
      showSizes(name, cached.path, token);
    }
    pending.value = cached ? null : place;
    loading.value = true;
    try {
      const reply = await api.remoteCall(name, { call: "list_dir", path: place });
      if (token !== generation || reply.reply !== "listing") return;
      const fresh = reply.listing;
      listings.set(key(name, place), fresh);
      listings.set(key(name, fresh.path), fresh);
      listing.value = fresh;
      if (!cached) {
        step(fresh.path);
        showSizes(name, fresh.path, token);
      }
    } catch (err) {
      if (token === generation) error.value = describeRemoteError(err, name);
    } finally {
      if (token === generation) {
        loading.value = false;
        pending.value = null;
      }
    }
  }

  function push(path: Place) {
    if (history.value[index.value] === path) return;
    history.value = [...history.value.slice(0, index.value + 1), path];
    index.value = history.value.length - 1;
  }

  /** Open a folder, adding it to history. */
  function open(path: string) {
    return go(path, push);
  }

  function back() {
    if (!canBack.value) return;
    const to = index.value - 1;
    return go(history.value[to], (path) => {
      history.value[to] = path;
      index.value = to;
    });
  }

  function forward() {
    if (!canForward.value) return;
    const to = index.value + 1;
    return go(history.value[to], (path) => {
      history.value[to] = path;
      index.value = to;
    });
  }

  /** The folder above, or the roots from a top folder. */
  function up() {
    if (!listing.value) return;
    return go(listing.value.parent, push);
  }

  /** Show the roots, as a step in history. */
  function home() {
    return go(null, push);
  }

  /** List the folder shown again, from the cache first if it is there. */
  function resume() {
    const current = history.value[index.value];
    const name = device.value;
    if (!name) return;
    return current ? go(current, () => {}) : loadRoots(name);
  }

  /** List the folder shown again, skipping the cache. */
  function refresh() {
    const current = history.value[index.value];
    const name = device.value;
    if (name && current) {
      listings.delete(key(name, current));
      sizesSeen.delete(key(name, current));
    }
    // The device may have updated since it said it cannot count sizes.
    if (name && noSizes.delete(name)) sizesSupported.value = true;
    return resume();
  }

  async function loadMore() {
    const name = device.value;
    const current = listing.value;
    if (!name || !current || current.next_cursor == null) return;
    const token = generation;
    loading.value = true;
    try {
      const reply = await api.remoteCall(name, {
        call: "list_dir",
        path: current.path,
        cursor: current.next_cursor,
      });
      if (token !== generation || reply.reply !== "listing") return;
      const merged = { ...reply.listing, entries: [...current.entries, ...reply.listing.entries] };
      listings.set(key(name, current.path), merged);
      listing.value = merged;
    } catch (err) {
      if (token === generation) error.value = describeRemoteError(err, name);
    } finally {
      if (token === generation) loading.value = false;
    }
  }

  function stopSizes() {
    if (sizeTimer !== undefined) window.clearTimeout(sizeTimer);
    sizeTimer = undefined;
    sizes.value = new Map();
    sizesDone.value = false;
  }

  /**
   * Show folder sizes for `path`: what was seen before at once, then ask the
   * device until it has counted everything. Stops when browsing moves on.
   */
  function showSizes(name: string, path: string, token: number) {
    if (!options.sizes || noSizes.has(name)) return;
    const seen = sizesSeen.get(key(name, path));
    if (seen) {
      sizes.value = seen.folders;
      sizesDone.value = seen.done;
    }
    const ask = async () => {
      sizeTimer = undefined;
      if (token !== generation) return;
      try {
        const reply = await api.remoteCall(name, { call: "folder_sizes", path });
        if (token !== generation || reply.reply !== "folder_sizes") return;
        const folders = new Map(reply.sizes.folders.map((f) => [f.path, f]));
        sizesSeen.set(key(name, path), { folders, done: reply.sizes.done });
        sizes.value = folders;
        sizesDone.value = reply.sizes.done;
        if (reply.sizes.done) return;
      } catch (err) {
        if (token !== generation) return;
        const code = err instanceof RemoteCallError ? err.code : null;
        // An older device answers `invalid`: it has no such call.
        if (code === "invalid") {
          noSizes.add(name);
          sizesSupported.value = false;
        }
        // Busy or slow: ask again. Anything else waits for the next visit.
        if (code !== "busy" && code !== "timeout") return;
      }
      if (token === generation) sizeTimer = window.setTimeout(ask, SIZE_POLL_MS);
    };
    void ask();
  }

  return {
    device,
    roots,
    listing,
    loading,
    error,
    pending,
    sizes,
    sizesDone,
    sizesSupported,
    canBack,
    canForward,
    choose,
    open,
    back,
    forward,
    up,
    home,
    resume,
    refresh,
    loadMore,
    stopSizes,
  };
}
