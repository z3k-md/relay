import { api } from "./api";
import type { ConfigChange, PathPreview, RemoteReply } from "./types";

/**
 * Add a device to a folder that already syncs here, choosing any folder on
 * that device, the same steps a folder pair runs on the destination (D39):
 * share it here, then on the other device join, create the folder if
 * needed, set online-only, and attach. A failed step undoes the earlier ones.
 */
export interface SetUpOnDevice {
  peer: string;
  /** This device's id: the other device joins from it. */
  thisDevice: string;
  spaceId: string;
  space: string;
  mount: string;
  path: string;
  onlineOnly: boolean;
}

function unexpected(reply: RemoteReply): Error {
  return new Error(`The other device answered "${reply.reply}" unexpectedly.`);
}

async function apply(peer: string, change: ConfigChange): Promise<void> {
  const reply = await api.remoteCall(peer, { call: "apply", change });
  if (reply.reply !== "applied") throw unexpected(reply);
}

export async function previewOn(peer: string, path: string): Promise<PathPreview> {
  const reply = await api.remoteCall(peer, { call: "preview", path });
  if (reply.reply !== "preview") throw unexpected(reply);
  return reply.preview;
}

/** Separator the other device writes paths with. */
export function separatorOf(path: string): string {
  return /^[A-Za-z]:\\/.test(path) || path.includes("\\") ? "\\" : "/";
}

function splitPath(path: string): { root: string; parts: string[] } {
  const sep = separatorOf(path);
  const parts = path.split(sep).filter(Boolean);
  if (sep === "\\") return { root: `${parts.shift() ?? ""}\\`, parts };
  return { root: "/", parts };
}

function joinPath(root: string, parts: string[]): string {
  const sep = root.endsWith("\\") ? "\\" : "/";
  return root + parts.join(sep);
}

/** Create `path` on the device, and any missing folders above it. */
async function ensureDir(peer: string, path: string): Promise<string> {
  const { root, parts } = splitPath(path);
  let existing = parts.length;
  // Walk up to the deepest folder that exists.
  while (existing > 0) {
    const preview = await previewOn(peer, joinPath(root, parts.slice(0, existing)));
    if (preview.exists) {
      if (!preview.is_dir) throw new Error(`${joinPath(root, parts.slice(0, existing))} is a file.`);
      break;
    }
    existing -= 1;
  }
  let current = joinPath(root, parts.slice(0, existing));
  for (const name of parts.slice(existing)) {
    const reply = await api.remoteCall(peer, { call: "create_dir", parent: current, name });
    if (reply.reply !== "created") throw unexpected(reply);
    current = reply.entry.path;
  }
  return current;
}

export async function setUpOnDevice(step: SetUpOnDevice): Promise<void> {
  const undo: (() => Promise<unknown>)[] = [];
  try {
    await api.share(step.space, step.peer);
    undo.push(() => api.unshare(step.space, step.peer));
    await apply(step.peer, {
      op: "join_space",
      space: step.spaceId,
      from_peer: step.thisDevice,
      wait_ms: 8000,
    });
    undo.push(() => apply(step.peer, { op: "delete_space", space: step.space }));
    const path = await ensureDir(step.peer, step.path);
    if (step.onlineOnly) {
      await apply(step.peer, {
        op: "set_folder_mode",
        space: step.space,
        mount: step.mount,
        path: "",
        mode: "demand",
      });
    }
    await apply(step.peer, { op: "add_mount", space: step.space, mount: step.mount, path });
  } catch (err) {
    for (const step of undo.reverse()) await step().catch(() => {});
    throw err;
  }
}

/**
 * Where a folder should land on another device: the same place under its
 * home folder when this one is under ours, else `Relay/<name>` there.
 */
export function suggestPath(localPath: string | null, remoteHome: string, name: string): string {
  const sep = separatorOf(remoteHome);
  const home = remoteHome.replace(/[\\/]+$/, "");
  const under = localPath?.match(/^(?:\/Users\/[^/]+|\/home\/[^/]+|[A-Za-z]:\\Users\\[^\\]+)[\\/](.+)$/);
  const rest = under ? under[1].split(/[\\/]/).filter(Boolean) : ["Relay", name];
  return [home, ...rest].join(sep);
}

export async function remoteHome(peer: string): Promise<string | null> {
  const reply = await api.remoteCall(peer, { call: "roots" });
  if (reply.reply !== "roots") return null;
  return (reply.roots.find((root) => root.name === "Home") ?? reply.roots[0])?.path ?? null;
}
