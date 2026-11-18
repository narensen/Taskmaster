// One compact lane per worker: its page, what it is doing now, how long it has been at it
// and the tokens it has used. Only the clock ticks; a lane redraws when its own worker
// changes. Clicking a lane opens that worker's log; the diff button opens its change.

import { memo, useEffect, useLayoutEffect, useRef, useState } from "react";
import gsap from "gsap";
import { GitCompare } from "lucide-react";
import { RoleMark } from "../marks";
import { ProviderLogo } from "../providers/logos";
import { shortSpan, tokens } from "../format";
import { motionOn } from "../prefs";
import { ease } from "../motion";
import type { PageState } from "../api";
import type { Surface } from "./model";

/** A lane's state: its agent's transcript when there is one, else what the run says of the page. */
function laneState(s: Surface, page: PageState | undefined): Surface["state"] {
  if (s.calls.length) return s.state;
  if (page === "passed") return "done";
  if (page === "failed") return "failed";
  if (page === "building" || page === "flagged") return "working";
  return "waiting";
}

const PAGE_STEP: Partial<Record<PageState, string>> = {
  passed: "Passed its checks",
  failed: "Did not pass its checks",
  flagged: "Flagged a question for the master",
  building: "Building",
};

function step(s: Surface, page: PageState | undefined): string {
  const call = s.calls[s.calls.length - 1];
  if (!call) return (page && PAGE_STEP[page]) ?? "Waiting for its brief";
  if (call.end) return call.end.ok === false ? (call.end.error?.split("\n")[0] ?? "Stopped with an error") : "Finished its turn";
  for (let i = call.items.length - 1; i >= 0; i--) {
    const item = call.items[i]!;
    if (item.t === "tool") return `${item.name}${item.target ? ` ${item.target.split("/").pop()}` : ""}`;
    if (item.t === "text" && item.text.trim()) return item.text.trim().split("\n").pop()!.slice(0, 120);
    if (item.t === "thought") return "Thinking";
    if (item.t === "life") return item.text;
  }
  return "Starting";
}

function span(s: Surface, now: number): number | null {
  const first = s.calls[0];
  if (!first) return null;
  const start = Date.parse(first.at);
  const last = s.calls[s.calls.length - 1]!;
  const end = last.end ? Date.parse(last.end.at) : now;
  return Number.isNaN(start) ? null : Math.max(0, end - start);
}

const STATE: Record<Surface["state"], string> = { waiting: "waiting", working: "working", done: "done", failed: "failed" };

const Lane = memo(function Lane({ s, page, now, onOpen, onDiff }: { s: Surface; page: PageState | undefined; now: number; onOpen: (id: string) => void; onDiff: (page: string) => void }) {
  const state = laneState(s, page);
  const ms = span(s, state === "working" ? now : 0);
  return (
    <div className={`lane lane-${state}`} data-lane={s.id}>
      <button type="button" className="lane-main" onClick={() => onOpen(s.id)} aria-label={`${s.title}: ${STATE[state]}. Open its log`}>
        <span className="lane-mark" aria-hidden>
          <RoleMark role={s.tag?.role === "mid" ? "mid" : "worker"} size={13} />
        </span>
        <span className="lane-name clip">{s.page}</span>
        <span className="lane-step clip">{step(s, page)}</span>
        <span className="lane-meta">
          {s.tag?.harness && <ProviderLogo id={s.tag.harness} size={11} title={s.tag.harness} />}
          <span className="lane-state">{STATE[state]}</span>
          <span className="lane-num">{ms !== null ? shortSpan(ms) : "–"}</span>
          <span className="lane-num">{s.tokens ? tokens(s.tokens) : "–"}</span>
        </span>
      </button>
      {s.page && (
        <button type="button" className="icon lane-diff" aria-label={`${s.page}: show its change`} title="Show its change" onClick={() => onDiff(s.page!)}>
          <GitCompare size={13} />
        </button>
      )}
    </div>
  );
});

export function Lanes({ workers, pages, onOpen, onDiff }: { workers: Surface[]; pages: Record<string, PageState>; onOpen: (id: string) => void; onDiff: (page: string) => void }) {
  const [now, setNow] = useState(() => Date.now());
  const box = useRef<HTMLDivElement>(null);
  const seen = useRef(0);
  const working = workers.some((w) => laneState(w, pages[w.page ?? ""]) === "working");
  useEffect(() => {
    if (!working) return;
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [working]);

  // Workers fan out from the master: new lanes slide in from the left one after another.
  useLayoutEffect(() => {
    const el = box.current;
    if (!el || workers.length <= seen.current) return;
    const fresh = [...el.querySelectorAll<HTMLElement>(".lane")].slice(seen.current);
    seen.current = workers.length;
    if (!motionOn() || !fresh.length) return;
    const tl = gsap.timeline();
    tl.fromTo(fresh, { opacity: 0, x: -14 }, { opacity: 1, x: 0, duration: 0.24, ease: ease.out, stagger: 0.035, clearProps: "opacity,transform" });
    // Finished, not dropped, if more lanes arrive mid-way, so none is left half-faded.
    return () => void tl.progress(1).kill();
  }, [workers.length]);

  if (!workers.length) return null;
  const done = workers.filter((w) => laneState(w, pages[w.page ?? ""]) === "done").length;
  return (
    <section className="lanes" aria-label="Workers" ref={box}>
      <div className="lanes-head">
        <span className="section-title">Workers</span>
        <span className="lanes-count">
          {done} of {workers.length} done
        </span>
      </div>
      {workers.map((s) => (
        <Lane key={s.id} s={s} page={pages[s.page ?? ""]} now={laneState(s, pages[s.page ?? ""]) === "working" ? now : 0} onOpen={onOpen} onDiff={onDiff} />
      ))}
    </section>
  );
}
