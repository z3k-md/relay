# Relay Explorer P0: WebView2 go/no-go spike

The first phase of rebuilding a Files-class file explorer in our Rust/Tauri
stack, Windows first. P0 answers one question before we build the rest: is a
WebView2 front end over a Rust shell layer fast enough, and can it take over
drag and drop completely? The full feature inventory and phases P1–P6 are in
the project's explorer plan (`design/explorer-plan-windows.md` in the project
files); this page covers only what the spike contains and how to judge it.

## Pass bars

| Bar | Target | How it is measured |
| --- | --- | --- |
| First paint | ≤ 150 ms for a 10k-item folder | `invoke` to the first painted frame that shows rows |
| Scrolling | 60 fps with 200k items | "Sweep": top to bottom in 5 s, so every frame shows new rows and icons; average fps and p95 frame time |
| Memory | ≤ 250 MB with 3 tabs | App process plus every WebView2 child process; working set and private working set are both reported |
| External drop | 1,000 files from Explorer, and an Outlook attachment | Lands in the folder under the pointer, through the shell's own copy engine |

If any bar fails by a wide margin on a mid-range Windows PC, we stop and
reconsider the UI layer before P1.

## How to run it

### The numbers, in one command

On Windows, from `apps/relay-desktop` in PowerShell:

```powershell
bun install
bun run sidecar
$env:RELAY_EXPLORER_BENCH = "1"; bun run tauri dev --release
```

`--release` builds the Rust side optimized, which is what we judge the bars
on; plain `bun run dev` works too, with a debug build.

The explorer window opens by itself and runs every timed bar with no
clicking:
- five first-paint runs on 10k items;
- a sweep and a smooth scroll through 200k items;
- a thumbnail grid sweep;
- memory with three tabs open.

It then writes the report to `%TEMP%\relay-explorer-bench\results.txt`,
prints it, and quits. Set the variable to a file path to write the report
somewhere else. Leave the window on screen while it runs. WebView2 stops
drawing a minimized or fully covered window, so the run pauses until the
window is visible again, and redoes any timing that overlapped the pause.
The first run creates the benchmark folders, which takes a while for 200k
files.

Memory is reported three ways, total and per process: working set (counts
DLL pages the WebView2 processes share once per process, so it overstates),
private working set (what Task Manager's Memory column shows) and private
commit. The three-tab reading is taken after 10 s and again after 60 s
idle, by when Chromium has dropped decoded thumbnails it no longer draws.
Then the run asks WebView2 for its Low memory target and reads again. After saving, it closes the window and
reads the app alone. To try Chromium switches on the benchmark window
only, set `RELAY_EXPLORER_WEBVIEW_ARGS`, for example
`--in-process-gpu --enable-features=NetworkServiceInProcess2`; the report
names the switches it ran with.

An installed Relay can stay running. In benchmark mode the app opens only
the explorer window and leaves everything else alone: it doesn't hand off
to the running copy, and it starts no sync engine or tray icon. It also
writes no settings and doesn't touch autostart, the CLI or updates.

`bun run sidecar` is a bash script. If `bash` on your PATH is the WSL stub
with no distro installed, put Git Bash first on PATH for that shell.

### By hand, and the checks that need a person

Run `bun run dev`, open the tray menu and pick **Explorer (preview)**. The
**Perf** panel on the right shows the pass bars live.

1. **Benchmark folders.** Click *10k files*, *200k files* or *2k images*.
   Each is created once under `%TEMP%\relay-explorer-bench` and reused.
   Creating the 200k folder the first time takes a while; the panel shows
   progress.
2. **First paint.** Open the 10k folder (or press F5 in it) and read *First
   paint*. Repeat a few times; the first run includes a cold disk cache.
3. **Scrolling.** In the 200k folder, details view, click *Sweep (5 s)*. Try
   the grid view on *2k images* too, which exercises thumbnails.
4. **Memory.** Open three tabs (Ctrl+T): the 200k folder, the 2k images in
   grid view, and your home folder. Read the working set after scrolling
   each.
5. **Drag and drop.** Drag 1,000 files from Explorer onto a folder row and
   onto empty space; drag an attachment out of Outlook. The panel lists the
   formats the source offered. Drag items out of Relay Explorer to the
   desktop. Ctrl copies, Shift moves, as in Explorer.
6. **Context menu.** Right-click items and empty space; Shift+right-click
   for extended verbs. Check that *Send to*, *Open with* and third-party
   entries (7-Zip, Git) render and work.
7. Click **Copy results** and paste them into the thread.

## What is in the spike

- `crates/relay-shell-win`, the only crate allowed `unsafe` (and only in its
  `win` module):
  - an STA thread pool;
  - `FindFirstFileExW` listing;
  - an overlapped `ReadDirectoryChangesW` watcher;
  - `IShellItemImageFactory` icons and thumbnails, encoded as PNG;
  - the real Explorer context menu (`IContextMenu`, with
    `IContextMenu2/3` message forwarding);
  - an `IDropTarget` that replaces WebView2's on its child windows;
  - `SHDoDragDrop` for drags out;
  - `IFileOperation` copy and move;
  - process-tree memory.
- `crates/relay-explorer`, platform-neutral. It provides:
  - entries;
  - timed listing batches (first at 25 ms, then every 150 ms);
  - watcher coalescing with rename pairing;
  - Explorer's drop-effect rules;
  - icon URL parsing;
  - benchmark folders.
- The desktop app adds:
  - an `explorer` window;
  - commands that stream over `tauri::ipc::Channel`;
  - `relay-icon` and `relay-thumb` URL schemes. Icons are cached per type
    in Rust and shared by URL in the webview, so a 200k folder fetches about
    a dozen icons.
- The Vue view:
  - tabs;
  - virtualized details and grid views (`@tanstack/vue-virtual`);
  - sorting with a numeric collator, which is close to `StrCmpLogicalW`;
  - live updates;
  - selection, keyboard navigation and the perf panel.

## Design choices worth checking

- **Drops never let the source delete.** When a drop is a move, we move
  through `IFileOperation` and report "copy" to the source. A source told
  "move" deletes its originals, which would race our own copy.
- **Virtual files are saved during the drop.** Outlook attachments and zip
  entries (`FileGroupDescriptorW` and `FileContents`) are written before the
  drop returns, because the source may free them right after. Names are
  confined to the target folder and never overwrite: `name (2).ext`.
- **Hit-testing stays in the web view.** The drop target asks the page which
  folder is under the pointer (`elementFromPoint`, then `data-drop-dir`). It
  then picks the effect with Explorer's rules: Ctrl copies, Shift moves, and
  with no key held, same volume moves and other volumes copy.
- **Disk calls stay off the main thread.** Tauri runs sync commands on the
  main thread, so every command that touches the disk is async. Menus and
  drag and drop run on the main thread on purpose, because OLE needs the
  window's thread.

## Known gaps, by design for P0

- Rename, delete, new folder and the clipboard are not wired yet (P1).
- Plain HTML5 drag and drop inside the page is disabled. Our drop target
  replaces WebView2's, as wry's does.
- Thumbnails rely on the shell's own cache and WebView2's image cache. There
  is no disk cache of our own yet.
- Long virtual-file drops run on the UI thread. That is fine for
  attachments, but not for a 2 GB zip entry.
