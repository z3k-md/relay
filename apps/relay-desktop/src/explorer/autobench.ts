import { Channel } from "@tauri-apps/api/core";
import { explorer } from "./api";
import { type ScrollResult, describeScroll, machineLines, runScroll } from "./bench";
import type { Folder } from "./folder";

/** What the one-shot benchmark needs from the explorer window. */
export interface BenchHost {
  home: string;
  active(): Folder;
  /** Open a tab on `path`, make it active and wait for its view to mount. */
  openTab(path: string): Promise<Folder>;
  scroller(): HTMLElement | null;
  tabs(): number;
  status(text: string): void;
}

const MB = 1024 * 1024;

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitDone(folder: Folder, timeoutMs = 300_000) {
  const until = performance.now() + timeoutMs;
  while (folder.loading.value || folder.stats.doneMs === null) {
    if (folder.error.value) throw new Error(`listing ${folder.path.value}: ${folder.error.value}`);
    if (performance.now() > until) throw new Error(`listing ${folder.path.value} timed out`);
    await sleep(25);
  }
}

function ms(v: number | null): string {
  return v === null ? "—" : v.toFixed(0);
}

function listingLine(label: string, f: Folder): string {
  const s = f.stats;
  return `${label}: ${s.total} items, first batch ${ms(s.firstBatchMs)} ms, first paint ${ms(s.firstPaintMs)} ms, all ${ms(s.doneMs)} ms (Rust ${ms(s.backendMs)} ms), sort/merge ${s.sortMs.toFixed(1)} ms`;
}

/**
 * The P0 pass bars, end to end, with no clicking: first paint of a 10k
 * folder (5 runs), sweep and smooth scroll of 200k items, a thumbnail grid
 * sweep, then memory with three tabs open. Returns the report.
 */
export async function autobench(host: BenchHost): Promise<string> {
  const lines = [`Relay Explorer P0 benchmark (${new Date().toISOString()})`, ...machineLines(), ""];
  const make = (kind: "files" | "images", count: number) => {
    const progress = new Channel<number>();
    progress.onmessage = (n) => host.status(`Creating ${kind}-${count}: ${n.toLocaleString()}`);
    host.status(`Creating ${kind}-${count}…`);
    return explorer.makeBench(kind, count, progress);
  };
  const scroll = async (folder: Folder, mode: "sweep" | "smooth") => {
    host.status(`Scrolling (${mode}) ${folder.path.value}…`);
    const el = host.scroller();
    if (!el) throw new Error("no list to scroll");
    return runScroll(el, mode, folder.rows.value.length);
  };

  try {
    const tab = host.active();
    const tenK = await make("files", 10_000);
    const paints: number[] = [];
    for (let run = 1; run <= 5; run++) {
      host.status(`First paint, run ${run} of 5…`);
      tab.navigate(tenK);
      await waitDone(tab);
      paints.push(tab.stats.firstPaintMs ?? Number.NaN);
      lines.push(listingLine(`10k run ${run}`, tab));
      await sleep(300);
    }
    const medianPaint = [...paints].sort((a, b) => a - b)[2];

    const big = await make("files", 200_000);
    tab.navigate(big);
    await waitDone(tab);
    lines.push(listingLine("200k", tab));
    await sleep(500);
    const sweep = await scroll(tab, "sweep");
    const smooth = await scroll(tab, "smooth");
    lines.push(`Details ${describeScroll(sweep)}`, `Details ${describeScroll(smooth)}`);
    const afterBig = await explorer.memory();

    const images = await make("images", 2_000);
    const grid = await host.openTab(images);
    grid.view.value = "grid";
    await waitDone(grid);
    await sleep(500);
    const gridSweep: ScrollResult = await scroll(grid, "sweep");
    lines.push(listingLine("2k images", grid), `Grid ${describeScroll(gridSweep)}`);

    const home = await host.openTab(host.home);
    await waitDone(home);
    host.status("Letting memory settle…");
    await sleep(3000);
    const memory = await explorer.memory();
    const mem = (m: typeof memory) =>
      m
        ? `working set ${(m.workingSetBytes / MB).toFixed(0)} MB, private ${(m.privateBytes / MB).toFixed(0)} MB, ${m.processes} processes`
        : "n/a";
    lines.push(`Memory after 200k scroll (1 tab): ${mem(afterBig)}`);
    lines.push(`Memory with ${host.tabs()} tabs: ${mem(memory)}`);

    const mark = (ok: boolean) => (ok ? "PASS" : "FAIL");
    lines.push(
      "",
      `${mark(medianPaint <= 150)} first paint ≤ 150 ms for 10k items: median ${medianPaint.toFixed(0)} ms (runs ${paints.map((p) => p.toFixed(0)).join(", ")})`,
      `${mark(sweep.avgFps >= 57 && sweep.p95 <= 20)} 60 fps sweeping 200k items: ${sweep.avgFps.toFixed(1)} fps, p95 ${sweep.p95.toFixed(1)} ms`,
      memory
        ? `${mark(memory.workingSetBytes <= 250 * MB)} ≤ 250 MB with ${host.tabs()} tabs: ${(memory.workingSetBytes / MB).toFixed(0)} MB`
        : "n/a memory (Windows only)",
    );
  } catch (err) {
    lines.push("", `Benchmark stopped: ${err}`);
  }
  return lines.join("\n");
}
