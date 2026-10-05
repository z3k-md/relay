export interface ScrollResult {
  mode: "smooth" | "sweep";
  rows: number;
  frames: number;
  avgFps: number;
  p95: number;
  p99: number;
  /** Frames that took over 1.5 refresh intervals. */
  dropped: number;
  worst: number;
  /** Frames painted with no rows where the list should show some. */
  blank: number;
}

export function nextFrame(): Promise<number> {
  return new Promise((resolve) => requestAnimationFrame(resolve));
}

/**
 * True when the list shows nothing at a few points down its middle: the
 * hit lands on the scroller or its full-height spacer instead of a row.
 */
function looksBlank(el: HTMLElement): boolean {
  const box = el.getBoundingClientRect();
  const x = box.left + Math.min(40, box.width / 2);
  for (const f of [0.25, 0.5, 0.75]) {
    const hit = document.elementFromPoint(x, box.top + box.height * f);
    if (hit === el || hit === el.firstElementChild) return true;
  }
  return false;
}

/**
 * Scroll `el` and time every frame. "smooth" scrolls at 3,000 px/s for
 * 10 s; "sweep" goes top to bottom in 5 s, so every frame shows rows (and
 * icons) it has not shown before.
 *
 * Each step dispatches the scroll event itself so the list renders that
 * frame's rows before it paints. Otherwise the event arrives a frame late
 * and a sweep, which jumps further than the list's overscan every frame,
 * times frames with no rows in them. Frames that still paint blank are
 * counted.
 */
export async function runScroll(
  el: HTMLElement,
  mode: "smooth" | "sweep",
  rows: number,
): Promise<ScrollResult> {
  el.scrollTop = 0;
  await nextFrame();
  await nextFrame();
  const max = el.scrollHeight - el.clientHeight;
  const duration = mode === "sweep" ? 5000 : 10000;
  const deltas: number[] = [];
  const start = await nextFrame();
  let prev = start;
  let blank = 0;
  for (;;) {
    const now = await nextFrame();
    deltas.push(now - prev);
    prev = now;
    const t = now - start;
    if (t >= duration) break;
    el.scrollTop = mode === "sweep" ? (t / duration) * max : (t * 3) % (max + 1);
    el.dispatchEvent(new Event("scroll"));
    // Once the list's update (a microtask) has run, the DOM is what this
    // frame will paint.
    await Promise.resolve();
    await Promise.resolve();
    if (looksBlank(el)) blank++;
  }
  const sorted = [...deltas].sort((a, b) => a - b);
  const at = (q: number) => sorted[Math.min(sorted.length - 1, Math.floor(q * sorted.length))];
  const interval = at(0.5);
  return {
    mode,
    rows,
    frames: deltas.length,
    avgFps: (deltas.length * 1000) / (prev - start),
    p95: at(0.95),
    p99: at(0.99),
    dropped: deltas.filter((d) => d > interval * 1.5).length,
    worst: sorted[sorted.length - 1],
    blank,
  };
}

export function describeScroll(r: ScrollResult): string {
  return `${r.mode}: ${r.rows} rows, ${r.frames} frames, ${r.avgFps.toFixed(1)} fps, p95 ${r.p95.toFixed(1)} ms, p99 ${r.p99.toFixed(1)} ms, dropped ${r.dropped}, worst ${r.worst.toFixed(1)} ms, blank ${r.blank}`;
}

export function machineLines(): string[] {
  return [
    `UA: ${navigator.userAgent}`,
    `Screen: ${screen.width}x${screen.height} @${window.devicePixelRatio}x, cores ${navigator.hardwareConcurrency}`,
  ];
}
