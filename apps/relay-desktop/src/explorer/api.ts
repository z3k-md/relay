import { Channel, convertFileSrc, invoke } from "@tauri-apps/api/core";

/** `[name, flags, size, modifiedMs]`, as relay-explorer serialises it. */
export type Entry = [name: string, flags: number, size: number, modifiedMs: number];

export const FLAG_DIR = 1;
export const FLAG_HIDDEN = 1 << 1;
export const FLAG_SYSTEM = 1 << 2;
export const FLAG_READONLY = 1 << 3;
export const FLAG_LINK = 1 << 4;
export const FLAG_CLOUD = 1 << 5;

export type Change =
  | { kind: "upsert"; entry: Entry }
  | { kind: "removed"; name: string }
  | { kind: "renamed"; from: string; entry: Entry }
  | { kind: "rescan" };

export type ListEvent =
  | { kind: "batch"; entries: Entry[] }
  | { kind: "done"; total: number; elapsedMs: number }
  | { kind: "error"; message: string }
  | { kind: "changes"; changes: Change[] };

export type DragEvent =
  | {
      kind: "over";
      /** Physical client pixels. */
      x: number;
      y: number;
      enter: boolean;
      count: number;
      virtualCount: number;
      formats: string[];
      effect: number;
    }
  | { kind: "leave" }
  | { kind: "dropped"; effect: number; target: string; count: number }
  | { kind: "done"; target: string; ok: boolean; message: string | null; elapsedMs: number };

export const EFFECT_COPY = 1;
export const EFFECT_MOVE = 2;

export interface Places {
  home: string;
  roots: string[];
  /** Shell icons, menus and drag and drop are available (Windows). */
  native: boolean;
}

export interface MemoryInfo {
  processes: number;
  privateBytes: number;
  workingSetBytes: number;
}

export const explorer = {
  places: () => invoke<Places>("explorer_places"),
  list: (tab: string, path: string, onEvent: Channel<ListEvent>) =>
    invoke<void>("explorer_list", { tab, path, onEvent }),
  closeTab: (tab: string) => invoke<void>("explorer_close_tab", { tab }),
  openItem: (path: string) => invoke<void>("explorer_open_item", { path }),
  contextMenu: (folder: string, names: string[], x: number, y: number, extended: boolean) =>
    invoke<string | null>("explorer_context_menu", { folder, names, x, y, extended }),
  ready: (onDrag: Channel<DragEvent>) => invoke<boolean>("explorer_ready", { onDrag }),
  dropTarget: (path: string | null) => invoke<void>("explorer_drop_target", { path }),
  startDrag: (folder: string, names: string[]) =>
    invoke<number>("explorer_start_drag", { folder, names }),
  memory: () => invoke<MemoryInfo | null>("explorer_memory"),
  makeBench: (kind: "files" | "images", count: number, onProgress: Channel<number>) =>
    invoke<string>("explorer_make_bench", { kind, count, onProgress }),
};

export function separator(path: string): string {
  return /^[A-Za-z]:/.test(path) || path.startsWith("\\\\") || path.includes("\\") ? "\\" : "/";
}

export function joinPath(dir: string, name: string): string {
  const sep = separator(dir);
  return dir.endsWith(sep) ? dir + name : dir + sep + name;
}

/** The parent folder, or null at a root (`C:\`, `\\server\share`, `/`). */
export function parentPath(path: string): string | null {
  const sep = separator(path);
  const trimmed = path.length > 1 && path.endsWith(sep) && !/^[A-Za-z]:\\$/.test(path)
    ? path.slice(0, -1)
    : path;
  if (/^[A-Za-z]:\\?$/.test(trimmed) || trimmed === "/") return null;
  if (/^\\\\[^\\]+\\[^\\]+$/.test(trimmed)) return null;
  const at = trimmed.lastIndexOf(sep);
  if (at < 0) return null;
  if (at === 0) return sep;
  const parent = trimmed.slice(0, at);
  return /^[A-Za-z]:$/.test(parent) ? parent + "\\" : parent;
}

export function baseName(path: string): string {
  const sep = separator(path);
  const trimmed = path.endsWith(sep) ? path.slice(0, -1) : path;
  return trimmed.slice(trimmed.lastIndexOf(sep) + 1) || path;
}

/** Same rules as relay_explorer::image_url::icon_cache_key. */
function iconKey(entry: Entry): string | null {
  const [name, flags] = entry;
  if (flags & FLAG_LINK) return null;
  if (flags & FLAG_DIR) return flags & (FLAG_READONLY | FLAG_SYSTEM) ? null : "<dir>";
  const dot = name.lastIndexOf(".");
  if (dot <= 0) return null;
  const ext = name.slice(dot + 1).toLowerCase();
  const ownIcon = ["exe", "ico", "lnk", "url", "cur", "ani", "scr", "msc", "appref-ms"];
  return ext && !ownIcon.includes(ext) ? "." + ext : null;
}

function imageUrl(scheme: string, path: string, entry: Entry, px: number): string {
  return `${convertFileSrc(path, scheme)}?s=${px}&f=${entry[1]}&m=${entry[3]}`;
}

const sharedIcons = new Map<string, string>();

/**
 * URL of an item's shell icon at `cssPx`. Items that share an icon share a
 * URL, so the webview fetches each kind once and serves the rest from cache.
 */
export function iconUrl(dir: string, entry: Entry, cssPx: number): string {
  const px = Math.round(cssPx * window.devicePixelRatio);
  const key = iconKey(entry);
  if (key) {
    const cacheKey = `${key}|${px}`;
    let url = sharedIcons.get(cacheKey);
    if (!url) {
      url = imageUrl("relay-icon", joinPath(dir, entry[0]), entry, px);
      sharedIcons.set(cacheKey, url);
    }
    return url;
  }
  return imageUrl("relay-icon", joinPath(dir, entry[0]), entry, px);
}

export function thumbUrl(dir: string, entry: Entry, cssPx: number): string {
  if (entry[1] & FLAG_DIR) return iconUrl(dir, entry, cssPx);
  const px = Math.round(cssPx * window.devicePixelRatio);
  return imageUrl("relay-thumb", joinPath(dir, entry[0]), entry, px);
}
