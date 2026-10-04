import { ref } from "vue";
import { api, RemoteCallError } from "./api";
import type { DirListing, RemoteRoot } from "./types";

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

/**
 * Browsing state for one paired device: its roots, the folder shown, and
 * paging. `listing` null means the roots are shown.
 */
export function useRemoteFolders() {
  const device = ref<string | null>(null);
  const roots = ref<RemoteRoot[]>([]);
  const listing = ref<DirListing | null>(null);
  const loading = ref(false);
  const error = ref<string | null>(null);

  async function run(action: (name: string) => Promise<void>) {
    const name = device.value;
    if (!name) return;
    loading.value = true;
    error.value = null;
    try {
      await action(name);
    } catch (err) {
      error.value = describeRemoteError(err, name);
    } finally {
      loading.value = false;
    }
  }

  async function choose(name: string) {
    device.value = name;
    listing.value = null;
    roots.value = [];
    await run(async (peer) => {
      const reply = await api.remoteCall(peer, { call: "roots" });
      if (reply.reply === "roots") roots.value = reply.roots;
    });
  }

  async function open(path: string) {
    await run(async (peer) => {
      const reply = await api.remoteCall(peer, { call: "list_dir", path });
      if (reply.reply === "listing") listing.value = reply.listing;
    });
  }

  async function loadMore() {
    const current = listing.value;
    if (!current || current.next_cursor == null) return;
    const cursor = current.next_cursor;
    await run(async (peer) => {
      const reply = await api.remoteCall(peer, { call: "list_dir", path: current.path, cursor });
      if (reply.reply === "listing") {
        listing.value = { ...reply.listing, entries: [...current.entries, ...reply.listing.entries] };
      }
    });
  }

  function up() {
    const parent = listing.value?.parent;
    if (parent) void open(parent);
    else listing.value = null;
  }

  return { device, roots, listing, loading, error, choose, open, loadMore, up };
}
