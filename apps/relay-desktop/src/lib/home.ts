import { reactive } from "vue";
import { api, errorText } from "./api";
import type {
  ConflictView,
  DeleteHold,
  MountView,
  OfferView,
  PeerView,
  SpaceView,
  TransferLive,
} from "./types";

/**
 * What the home screen shows, loaded together so every section agrees.
 * Each read is O(folders + devices) or a prefiltered SQL query, never a
 * walk of the object store, so polling it stays cheap.
 */
export const home = reactive({
  spaces: [] as SpaceView[],
  peers: [] as PeerView[],
  offers: [] as OfferView[],
  holds: [] as DeleteHold[],
  conflicts: [] as ConflictView[],
  loaded: false,
  error: null as string | null,
});

let generation = 0;

export async function loadHome(): Promise<void> {
  const gen = ++generation;
  try {
    const [spaces, peers, offers, holds, conflicts] = await Promise.all([
      api.listSpaces(),
      api.listPeers(),
      api.listOffers(),
      api.listDeleteHolds(),
      api.listConflicts(),
    ]);
    if (gen !== generation) return;
    Object.assign(home, { spaces, peers, offers, holds, conflicts, loaded: true, error: null });
  } catch (err) {
    if (gen !== generation) return;
    home.error = errorText(err);
  }
}

/** A synced folder: one mount of a space. Spaces stay out of the UI. */
export interface Folder {
  space: SpaceView;
  mount: MountView;
  /** Unique within this device. */
  key: string;
  /** The folder of the same space this one sits inside on this device (D54). */
  inside: string | null;
}

export function folderKey(space: string, mount: string): string {
  return `${space}/${mount}`;
}

/** Compare paths the way the OS that wrote them would: Windows ignores case. */
function pathKey(path: string): string {
  const windows = /^[A-Za-z]:|\\/.test(path);
  const p = path.replace(/\\/g, "/").replace(/\/+$/, "");
  return windows ? p.toLowerCase() : p;
}

/** The closest other attached mount of the space whose folder contains this one. */
function enclosing(space: SpaceView, mount: MountView): string | null {
  if (!mount.path) return null;
  const own = pathKey(mount.path);
  let best: MountView | null = null;
  for (const other of space.mounts) {
    if (other.name === mount.name || !other.path) continue;
    const root = pathKey(other.path);
    if (own.startsWith(`${root}/`) && (!best || root.length > pathKey(best.path!).length)) best = other;
  }
  return best?.name ?? null;
}

/** Every folder, with one nested in another of its space listed right after it. */
export function folders(): Folder[] {
  const flat = home.spaces.flatMap((space) =>
    space.mounts.map((mount) => ({
      space,
      mount,
      key: folderKey(space.name, mount.name),
      inside: enclosing(space, mount),
    })),
  );
  const out: Folder[] = [];
  const place = (f: Folder) => {
    out.push(f);
    for (const child of flat) {
      if (child.space === f.space && child.inside === f.mount.name) place(child);
    }
  };
  for (const f of flat) if (!f.inside) place(f);
  return out;
}

/** How many folders up a folder is nested on this device. */
export function depth(folder: Folder): number {
  let n = 0;
  let at: Folder | null = folder;
  while (at?.inside && n < 8) {
    at = findFolder(at.space.name, at.inside);
    n++;
  }
  return n;
}

export function findFolder(space: string, mount: string): Folder | null {
  return folders().find((f) => f.key === folderKey(space, mount)) ?? null;
}

/** The folder's other mounts in the same space, which share its devices. */
export function siblings(folder: Folder): string[] {
  return folder.space.mounts.filter((m) => m.name !== folder.mount.name).map((m) => m.name);
}

/** Folders of the same space sitting directly inside this one here (D54). */
export function nestedIn(folder: Folder): string[] {
  return folders()
    .filter((f) => f.space === folder.space && f.inside === folder.mount.name)
    .map((f) => f.mount.name);
}

/** A mount name not used yet in this space. */
export function freeMountName(space: SpaceView | undefined, wanted: string): string {
  const taken = new Set(space?.mounts.map((m) => m.name) ?? []);
  if (!taken.has(wanted)) return wanted;
  for (let n = 2; ; n++) {
    const next = `${wanted} ${n}`;
    if (!taken.has(next)) return next;
  }
}

export function pendingHolds(): DeleteHold[] {
  return home.holds.filter((h) => !h.decision);
}

export function folderConflicts(folder: Folder): ConflictView[] {
  return home.conflicts.filter((c) => c.space === folder.space.name && c.mount === folder.mount.name);
}

export function folderTransfers(folder: Folder, transfers: TransferLive[]): TransferLive[] {
  return transfers.filter(
    (t) => t.space === folder.space.name && (t.mount == null || t.mount === folder.mount.name),
  );
}

export type Tone = "ok" | "sync" | "warn" | "off";

/** One line about a folder, and the colour of its dot. */
export function folderStatus(
  folder: Folder,
  transfers: TransferLive[],
): { text: string; tone: Tone } {
  const { space, mount } = folder;
  if (!mount.attached) return { text: "Not on this device", tone: "off" };
  const held = pendingHolds().filter((h) => h.space === space.name && h.mount === mount.name);
  if (held.length) return { text: "Deletes waiting for you", tone: "warn" };
  const conflicts = folderConflicts(folder).length;
  if (conflicts) {
    return { text: conflicts === 1 ? "1 conflict" : `${conflicts} conflicts`, tone: "warn" };
  }
  if (mount.state && mount.state !== "OK") return { text: mount.state, tone: "warn" };
  const live = folderTransfers(folder, transfers);
  const index = live.find((t) => t.direction === "index");
  if (index) return { text: `Indexing ${index.filesDone.toLocaleString()} files`, tone: "sync" };
  const moving = live.filter((t) => t.direction !== "index");
  if (moving.length) {
    const files = moving.reduce(
      (n, t) => n + Math.max(0, (t.filesTotal ?? t.filesDone) - t.filesDone),
      0,
    );
    const first = moving[0];
    const verb = first.direction === "receive" ? "Receiving from" : "Sending to";
    const what = files > 0 ? `${files.toLocaleString()} ${files === 1 ? "file" : "files"}` : "";
    return { text: [`${verb} ${first.peerName}`, what].filter(Boolean).join(" · "), tone: "sync" };
  }
  if (space.sharedWith.length === 0) return { text: "Only on this device", tone: "off" };
  const online = home.peers.filter((p) => p.connected && space.sharedWith.includes(p.name));
  if (online.length === 0) return { text: "Waiting for a device to come online", tone: "off" };
  return { text: "Up to date", tone: "ok" };
}

export function devicesLabel(names: string[]): string {
  if (names.length === 0) return "Not shared yet";
  return `With ${names.join(", ")}`;
}

/** Last path component, either separator. */
export function baseName(path: string): string {
  const parts = path.replace(/\\/g, "/").split("/").filter(Boolean);
  return parts[parts.length - 1] ?? path;
}

/** A space name not used yet on this device: "Photos", then "Photos 2". */
export function freeName(wanted: string): string {
  const taken = new Set(home.spaces.map((s) => s.name));
  if (!taken.has(wanted)) return wanted;
  for (let n = 2; ; n++) {
    const next = `${wanted} ${n}`;
    if (!taken.has(next)) return next;
  }
}

export function formatAgo(ms: number, now = Date.now()): string {
  const sec = Math.max(0, Math.floor((now - ms) / 1000));
  if (sec < 60) return "just now";
  const min = Math.floor(sec / 60);
  if (min < 60) return `${min} min ago`;
  const hours = Math.floor(min / 60);
  if (hours < 24) return hours === 1 ? "1 hour ago" : `${hours} hours ago`;
  const days = Math.floor(hours / 24);
  return days === 1 ? "yesterday" : `${days} days ago`;
}

export function peerLine(peer: PeerView): string {
  if (peer.connected) return "Online";
  if (peer.lastSeenMs != null) return `Seen ${formatAgo(peer.lastSeenMs)}`;
  return "Not seen yet";
}

export function sharedWithPeer(peer: PeerView): Folder[] {
  return folders().filter((f) => f.space.sharedWith.includes(peer.name));
}
