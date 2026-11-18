// The run as a line in time: planning, each worker, the checks, and the end. Drag the
// handle (or use the arrow keys) to replay the run as it was at that moment; the agents
// below show only what had happened by then. "Live" goes back to now.

import { useEffect, useMemo, useRef, useState } from "react";
import type { AgentEntry, RunDetail } from "../api";
import { shortSpan } from "../format";

export interface Moment {
  at: number;
  label: string;
  kind: "plan" | "worker" | "check" | "done";
}

export function moments(detail: RunDetail, entries: AgentEntry[]): { start: number; end: number; marks: Moment[] } {
  const marks: Moment[] = [];
  let start = Infinity;
  let end = 0;
  const firstOf = new Map<string, number>();
  for (const e of entries) {
    const at = Date.parse(e.at);
    if (Number.isNaN(at)) continue;
    start = Math.min(start, at);
    end = Math.max(end, at);
    const id = e.agent.id.split("/")[0]!;
    if (e.kind === "start" && !firstOf.has(id)) {
      firstOf.set(id, at);
      if (id === "master") marks.push({ at, label: "Planning", kind: "plan" });
      else if (id.startsWith("page:")) marks.push({ at, label: id.slice(5), kind: "worker" });
      else if (id === "reviewer" || id === "verifier") marks.push({ at, label: id === "reviewer" ? "Review" : "Verify", kind: "check" });
    }
    if (e.kind === "preview") marks.push({ at, label: "Browser check", kind: "check" });
  }
  if (!Number.isFinite(start)) return { start: 0, end: 0, marks: [] };
  if ((detail.phase === "done" || detail.phase === "failed") && !detail.active) marks.push({ at: end, label: detail.phase === "done" ? "Done" : "Stopped", kind: "done" });
  return { start, end, marks };
}

export function Timeline({
  detail,
  entries,
  at,
  onScrub,
}: {
  detail: RunDetail;
  entries: AgentEntry[];
  /** The moment shown, or null for live. */
  at: number | null;
  onScrub: (at: number | null) => void;
}) {
  const { start, end, marks } = useMemo(() => moments(detail, entries), [detail, entries]);
  const track = useRef<HTMLDivElement>(null);
  // The handle moves by transform, so its track's width is measured rather than laid out.
  const [width, setWidth] = useState(0);
  useEffect(() => {
    const el = track.current;
    if (!el) return;
    const observer = new ResizeObserver(() => setWidth(el.clientWidth));
    observer.observe(el);
    setWidth(el.clientWidth);
    return () => observer.disconnect();
  }, [marks.length > 0]);
  if (!marks.length || end <= start) return null;
  const length = end - start;
  const pos = at === null ? 1 : Math.min(1, Math.max(0, (at - start) / length));
  const fromX = (clientX: number) => {
    const rect = track.current!.getBoundingClientRect();
    const f = Math.min(1, Math.max(0, (clientX - rect.left) / rect.width));
    return f >= 0.999 ? null : start + f * length;
  };
  const drag = (e: React.PointerEvent) => {
    e.preventDefault();
    (e.target as HTMLElement).setPointerCapture?.(e.pointerId);
    onScrub(fromX(e.clientX));
    let frame = 0;
    const move = (ev: PointerEvent) => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => onScrub(fromX(ev.clientX)));
    };
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  };
  const key = (e: React.KeyboardEvent) => {
    const stepMs = Math.max(1000, length / 50);
    const cur = at ?? end;
    let next: number | null = cur;
    if (e.key === "ArrowLeft") next = Math.max(start, cur - stepMs);
    else if (e.key === "ArrowRight") next = cur + stepMs >= end ? null : cur + stepMs;
    else if (e.key === "Home") next = start;
    else if (e.key === "End" || e.key === "Escape") next = null;
    else return;
    e.preventDefault();
    onScrub(next);
  };
  return (
    <section className={`timeline ${at !== null ? "replaying" : ""}`} aria-label="Run timeline">
      <div className="tl-head">
        <span className="section-title">Timeline</span>
        <span className="tl-when">{at === null ? (detail.active ? "Live" : `${shortSpan(length)} in all`) : `Replaying at ${shortSpan(at - start)} of ${shortSpan(length)}`}</span>
        {at !== null && (
          <button type="button" className="ghost small" onClick={() => onScrub(null)}>
            Back to live
          </button>
        )}
      </div>
      <div className="tl-track" ref={track} onPointerDown={drag}>
        <div className="tl-rail" />
        <div className="tl-fill" style={{ transform: `scaleX(${pos})` }} />
        {marks.map((m, i) => (
          <span key={i} className={`tl-mark tl-${m.kind}`} style={{ left: `${((m.at - start) / length) * 100}%` }} title={`${m.label} · ${shortSpan(m.at - start)}`} />
        ))}
        <span
          className="tl-thumb"
          role="slider"
          tabIndex={0}
          aria-label="Replay the run at"
          aria-valuemin={0}
          aria-valuemax={Math.round(length / 1000)}
          aria-valuenow={Math.round(((at ?? end) - start) / 1000)}
          aria-valuetext={at === null ? "live" : shortSpan(at - start)}
          style={{ transform: `translateX(${pos * width}px)` }}
          onKeyDown={key}
        />
      </div>
      <div className="tl-labels" aria-hidden>
        {marks
          .filter((m) => m.kind !== "worker")
          .map((m, i) => (
            <span key={i} style={{ left: `${((m.at - start) / length) * 100}%` }}>
              {m.label}
            </span>
          ))}
      </div>
    </section>
  );
}
