import { convertFileSrc } from "@tauri-apps/api/core";
import type { FileRow } from "./types";

/** Image types the app makes thumbnails of (see src-tauri/src/thumbs.rs). */
const IMAGE_EXT = new Set(["jpg", "jpeg", "png", "webp", "bmp", "gif"]);

/** Whether a row can show a thumbnail: an image whose bytes are here. */
export function hasThumb(row: FileRow): boolean {
  if (row.kind !== "file" || row.state !== "local") return false;
  const dot = row.name.lastIndexOf(".");
  return dot > 0 && IMAGE_EXT.has(row.name.slice(dot + 1).toLowerCase());
}

/**
 * The thumbnail URL for a row shown `cssPx` wide. Size and time are in it
 * so an edited file gets a new URL instead of a stale cached image.
 */
export function thumbUrl(space: string, mount: string, row: FileRow, cssPx: number): string {
  const px = Math.ceil(cssPx * (window.devicePixelRatio || 1));
  const query = new URLSearchParams({
    space,
    mount,
    path: row.path,
    px: String(px),
    v: `${row.size ?? 0}-${row.modified_ms}`,
  });
  return `${convertFileSrc("t", "relay-thumb")}?${query}`;
}

export type FilesLayout = "list" | "thumbs" | "grid";
export type GridSize = "small" | "medium" | "large";

/** Tile edge in CSS pixels for each grid size. */
export const GRID_PX: Record<GridSize, number> = { small: 96, medium: 144, large: 224 };
/** Thumbnail edge in the list with thumbnails. */
export const LIST_THUMB_PX = 32;

export interface FilesViewPrefs {
  layout: FilesLayout;
  gridSize: GridSize;
}

const PREFS_KEY = "relay.files.view";
const DEFAULT_PREFS: FilesViewPrefs = { layout: "thumbs", gridSize: "medium" };

/** This device's choice of Files layout. Storage can be unavailable; then the default. */
export function loadFilesPrefs(): FilesViewPrefs {
  try {
    const saved = JSON.parse(localStorage.getItem(PREFS_KEY) ?? "null") as Partial<FilesViewPrefs> | null;
    const layout = saved?.layout;
    const gridSize = saved?.gridSize;
    return {
      layout: layout === "list" || layout === "thumbs" || layout === "grid" ? layout : DEFAULT_PREFS.layout,
      gridSize: gridSize && gridSize in GRID_PX ? gridSize : DEFAULT_PREFS.gridSize,
    };
  } catch {
    return { ...DEFAULT_PREFS };
  }
}

export function saveFilesPrefs(prefs: FilesViewPrefs): void {
  try {
    localStorage.setItem(PREFS_KEY, JSON.stringify(prefs));
  } catch {
    // Not remembered this time; the view still changes.
  }
}
