// One agent: who it is, what it is working on, what it says as it says it, and every tool
// call with the gate's answer. A worker that has not started stays on screen as waiting.

import { useEffect, useState } from "react";
import { motion } from "framer-motion";
import { Bot, Crown, ScanSearch, ShieldCheck } from "lucide-react";
import { api, type Attempt, type RunDetail } from "../api";
import { KnownPath, Rich, looksLikePath, masked } from "../paths";
import { emit, toast } from "../overlay";
import { tokens, usd } from "../format";
import { bezier, fadeUp, stagger } from "../motion";
import { motionOn } from "../prefs";
import type { Item, Surface } from "./model";

function byline(s: Surface, detail: RunDetail | null): { harness: string; model: string } {
  const roles = detail?.roles;
  const models = detail?.models ?? {};
  if (s.tag?.harness) return { harness: s.tag.harness, model: s.tag.model ?? "default model" };
  const role = s.page ? (detail?.pages.find((p) => p.id === s.page)?.tier ?? "worker") : s.role;
  const key = (["worker", "mid", "master", "reviewer", "verifier"].includes(role) ? role : "master") as keyof NonNullable<typeof roles>;
  return { harness: roles?.[key] ?? "?", model: (models as Record<string, string | null | undefined>)[key] ?? "default model" };
}

function ToolRow({ item }: { item: Extract<Item, { t: "tool" }> }) {
  const decision = item.allowed === undefined ? "asked" : item.allowed ? "allow" : "deny";
  const target = item.target ?? "";
  return (
    <div className={`tool ${decision}`}>
      <span className="tool-name">{item.name}</span>
      <span className="tool-target clip">
        {target && (looksLikePath(target) && !target.includes(" ") ? <KnownPath path={target} /> : <Rich text={target} />)}
      </span>
      <span className="tool-decision">{decision}</span>
    </div>
  );
}

function copyText(text: string) {
  void navigator.clipboard?.writeText(masked(text)).then(
    () => toast("Copied as Markdown", undefined, 2000),
    () => toast("Could not copy"),
  );
}

function Attempts({ run, page, events }: { run: string; page: string; events: number }) {
  const [list, setList] = useState<Attempt[]>([]);
  useEffect(() => {
    api.attempts(run, page).then(setList).catch(() => setList([]));
  }, [run, page, events]);
  if (!list.length) return null;
  return (
    <details className="attempts">
      <summary>
        {list.length} attempt{list.length === 1 ? "" : "s"} kept for this page
      </summary>
      <ol>
        {list.map((a, i) => (
          <li key={i} className={`attempt ${a.outcome}`}>
            <div className="row between gap">
              <span>
                #{i + 1} <span className="strong">{a.outcome}</span> · {a.tier} · {a.model}
              </span>
              {a.diff && (
                <button className="ghost small" onClick={() => emit("diff", { scope: "attempt", page, attempt: i })}>
                  diff
                </button>
              )}
            </div>
            {a.failure && (
              <pre className="pre small">
                <Rich text={a.failure} />
              </pre>
            )}
            {a.files.length > 0 && (
              <div className="small">
                {a.files.map((f) => (
                  <span key={f} className="file-tag">
                    <KnownPath path={f} />
                  </span>
                ))}
              </div>
            )}
          </li>
        ))}
      </ol>
    </details>
  );
}

export function AgentSurface({
  surface,
  detail,
  maxTokens,
  events,
  focused,
  index = 0,
  children,
}: {
  surface: Surface;
  detail: RunDetail | null;
  maxTokens: number;
  events: number;
  focused: boolean;
  index?: number;
  children?: React.ReactNode;
}) {
  const s = surface;
  const { harness, model } = byline(s, detail);
  const glob = s.tag?.owned_glob ?? detail?.plan?.pages.find((p) => p.id === s.page)?.owned_glob ?? null;
  const route = detail?.plan?.pages.find((p) => p.id === s.page)?.route;
  const units = detail?.plan?.pages.find((p) => p.id === s.page)?.units?.length;
  const [showBrief, setShowBrief] = useState(false);
  const on = motionOn();
  return (
    <motion.section
      layout="position"
      initial={on ? fadeUp.initial : false}
      animate={fadeUp.animate}
      transition={{
        duration: on ? fadeUp.transition.duration : 0,
        ease: bezier.out,
        delay: on ? stagger(index) : 0,
        layout: { duration: on ? 0.18 : 0, ease: bezier.out },
      }}
      className={`agent state-${s.state} ${focused ? "focused" : ""}`}
      id={`agent-${s.id}`}
      data-agent={s.id}
      tabIndex={-1}
    >
      <header className="agent-head">
        <div className="agent-title">
          <span className="agent-avatar" aria-hidden>
            {s.id === "master" ? <Crown size={14} /> : s.id === "reviewer" ? <ScanSearch size={14} /> : s.id === "verifier" ? <ShieldCheck size={14} /> : <Bot size={14} />}
          </span>
          <span className="strong grow clip">{s.title}</span>
          <span className="agent-state">
            <span className="agent-state-dot" aria-hidden />
            {s.state}
          </span>
        </div>
        <div className="agent-byline muted small">
          {harness} · {model}
          {s.cost > 0 && ` · ${usd(s.cost)}`}
          {s.calls_billed > 0 && ` · ${s.calls_billed} call${s.calls_billed === 1 ? "" : "s"}`}
        </div>
        <dl className="agent-facts small">
          {s.page && (
            <>
              <dt>page</dt>
              <dd>
                {s.page}
                {route ? ` ${route}` : ""}
                {(units ?? 0) > 1 ? ` (${units} workers)` : ""}
              </dd>
            </>
          )}
          {glob && (
            <>
              <dt>owns</dt>
              <dd className="mono">{glob}</dd>
            </>
          )}
          {s.tag?.worktree && (
            <>
              <dt>worktree</dt>
              <dd className="break">{s.tag.worktree}</dd>
            </>
          )}
          {(s.tag?.branch ?? detail?.branch) && (
            <>
              <dt>branch</dt>
              <dd>{s.tag?.branch ?? detail?.branch}</dd>
            </>
          )}
        </dl>
        {s.usage && (
          <div className="meter" title={`${s.tokens} tokens reported by ${harness}`}>
            <div className="meter-fill" style={{ width: `${Math.max(2, (100 * s.tokens) / Math.max(1, maxTokens))}%` }} />
            <span className="meter-label small">{tokens(s.tokens)} tok</span>
          </div>
        )}
      </header>
      {children}
      <div className="transcript" data-agent-text={s.id}>
        {s.calls.length === 0 && <div className="waiting">waiting</div>}
        {s.calls.map((call) => (
          <div key={call.call} className="call">
            <div className="call-head small muted">
              <span>
                {call.tag.role}
                {call.tag.page ? ` · ${call.tag.page}` : ""}
              </span>
              <span>
                {call.end ? (call.end.ok ? "finished" : "stopped") : "live"}
                {call.end?.seconds !== undefined && ` · ${call.end.seconds.toFixed(1)}s`}
                {call.brief && (
                  <button className="link small" onClick={() => setShowBrief(!showBrief)}>
                    brief
                  </button>
                )}
              </span>
            </div>
            {showBrief && call.brief && (
              <pre className="pre brief small">
                <Rich text={call.brief} />
              </pre>
            )}
            {call.items.map((item, i) =>
              item.t === "tool" ? (
                <ToolRow key={i} item={item} />
              ) : item.t === "life" ? (
                <div key={i} className="muted small" data-testid="agent-life">
                  {item.text}
                </div>
              ) : item.t === "thought" ? (
                <div key={i} className="thought">
                  <Rich text={item.text} />
                </div>
              ) : (
                <div key={i} className="said">
                  <div className="said-text">
                    <Said text={item.text} done={!!call.end} />
                    {!call.end && i === call.items.length - 1 && <span className="caret" aria-hidden />}
                  </div>
                  <button className="link small copy" onClick={() => copyText(item.text)}>
                    copy
                  </button>
                </div>
              ),
            )}
            {call.end?.error && (
              <div className="error-line">
                <Rich text={call.end.error} />
              </div>
            )}
          </div>
        ))}
      </div>
      {s.page && <Attempts run={detail?.id ?? ""} page={s.page} events={events} />}
    </motion.section>
  );
}

/** A finished reply that ends in a large JSON answer (the plan, a review) folds that part away. */
function Said({ text, done }: { text: string; done: boolean }) {
  const at = done ? text.search(/(^|\n)\s*\{/) : -1;
  const tail = at >= 0 ? text.slice(at).trim() : "";
  let structured = false;
  if (tail.length > 400) {
    try {
      structured = typeof JSON.parse(tail) === "object";
    } catch {
      structured = false;
    }
  }
  if (!structured) return <Rich text={text} />;
  const prose = text.slice(0, at).trim();
  return (
    <>
      {prose && <Rich text={prose} />}
      <details className="structured">
        <summary className="small muted">structured answer · {tail.split("\n").length} lines</summary>
        <div className="pre small">
          <Rich text={tail} />
        </div>
      </details>
    </>
  );
}
