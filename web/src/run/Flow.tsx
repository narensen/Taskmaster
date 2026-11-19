// One transcript. The person speaks, the master thinks and locks a plan, then each worker
// follows in order. Role names are plain text. Nothing sits in a card.

import { useEffect, useState, type ReactNode } from "react";
import { FileText, Globe, Pencil, Search, SquareTerminal } from "lucide-react";
import type { Phase, Plan, RunDetail, ToolApproval } from "../api";
import { tokens } from "../format";
import { KnownPath, Rich, looksLikePath } from "../paths";
import type { Call, Item, Surface, ToolItem } from "./model";

const VERB: [RegExp, string, "read" | "edit" | "run" | "search" | "look"][] = [
  [/edit|write|patch|strreplace|apply_patch/, "Edit", "edit"],
  [/bash|shell|command|exec|terminal/, "Run", "run"],
  [/search|grep|find|rg\b/, "Search", "search"],
  [/browser|preview|screenshot|navigate/, "Look", "look"],
  [/read|cat\b|open/, "Read", "read"],
];

function verbOf(name: string): { label: string; kind: "read" | "edit" | "run" | "search" | "look" } {
  const n = name.toLowerCase();
  for (const [re, label, kind] of VERB) if (re.test(n)) return { label, kind };
  return { label: name, kind: "read" };
}

function Glyph({ kind }: { kind: "read" | "edit" | "run" | "search" | "look" }) {
  const Icon = kind === "edit" ? Pencil : kind === "run" ? SquareTerminal : kind === "search" ? Search : kind === "look" ? Globe : FileText;
  return <Icon size={13} strokeWidth={1.5} aria-hidden />;
}

/** The first line of a thought, short enough to stand in for the block once it is done. */
export function thinkTitle(text: string): string {
  const line = text
    .trim()
    .split("\n")
    .map((s) => s.trim())
    .find(Boolean);
  if (!line) return "Thinking";
  return line.length > 88 ? `${line.slice(0, 87).trimEnd()}…` : line;
}

function stepSeconds(call: Call, index: number): string | null {
  const item = call.items[index];
  if (!item) return null;
  const next = call.items[index + 1]?.at ?? call.end?.at;
  if (!next) return null;
  const ms = Date.parse(next) - Date.parse(item.at);
  if (!Number.isFinite(ms) || ms < 0) return null;
  if (ms < 1000) return `${Math.max(1, Math.round(ms))}ms`;
  const s = ms / 1000;
  return `${s.toFixed(s < 10 ? 1 : 0)}s`;
}

/** A path the e key can open: the latest tool target that names a file. */
export function fileTarget(surface: Surface): string | null {
  for (let c = surface.calls.length - 1; c >= 0; c--) {
    const call = surface.calls[c];
    if (!call) continue;
    for (let i = call.items.length - 1; i >= 0; i--) {
      const item = call.items[i];
      if (item?.t === "tool" && item.target && looksLikePath(item.target.split(/\s+/)[0] ?? "")) return item.target.split(/\s+/)[0]!;
    }
  }
  return null;
}

function statusWord(surface: Surface, phase: Phase | undefined): string {
  if (surface.id === "master" && phase === "planning" && surface.state !== "failed") return "planning";
  if (surface.id === "master" && phase === "awaiting_approval" && surface.state !== "failed") return "asking";
  if (surface.state === "working") return "working";
  if (surface.state === "failed") return "failed";
  if (surface.state === "done") return "done";
  return "waiting";
}

function Think({ text, live, force }: { text: string; live: boolean; force: boolean }) {
  const [open, setOpen] = useState(live);
  useEffect(() => {
    setOpen(live);
  }, [live]);
  const shown = force || open;
  return (
    <div className={`think ${shown ? "open" : ""} ${live ? "live" : ""}`}>
      <button type="button" className="think-title" aria-expanded={shown} onClick={() => setOpen((v) => !v)}>
        {thinkTitle(text)}
      </button>
      {shown && (
        <div className="think-body">
          <Rich text={text} />
          {live && <span className="caret" aria-hidden />}
        </div>
      )}
    </div>
  );
}

function Step({ item, seconds, force }: { item: ToolItem; seconds: string | null; force: boolean }) {
  const [open, setOpen] = useState(false);
  const shown = force || open;
  const { label, kind } = verbOf(item.name);
  const target = item.target ?? "";
  const head = target.split(/\s+/)[0] ?? "";
  const input = item.input === undefined ? "" : typeof item.input === "string" ? item.input : JSON.stringify(item.input, null, 2);
  return (
    <div className={`step ${item.allowed === false ? "denied" : ""}`}>
      <button type="button" className="step-line" aria-expanded={shown} onClick={() => setOpen((v) => !v)}>
        <Glyph kind={kind} />
        <span className="step-name">{label}</span>
      </button>
      <span className="step-target clip" title={target}>
        {target && looksLikePath(head) ? <KnownPath path={head} /> : target}
      </span>
      <span className="step-dur">{item.allowed === false ? "refused" : (seconds ?? "")}</span>
      {shown && (input || item.output || item.diff) && (
        <div className="step-body">
          {input && <pre className="step-io">{input}</pre>}
          {item.output && (
            <pre className="step-io">
              <Rich text={item.output} />
            </pre>
          )}
          {item.diff && <pre className="step-io">{item.diff}</pre>}
        </div>
      )}
    </div>
  );
}

/** A finished reply that ends in a large JSON answer folds that part away. */
function Said({ text, done }: { text: string; done: boolean }) {
  const at = done ? text.search(/(^|\n)\s*[{[]/) : -1;
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
        <pre className="step-io">
          <Rich text={tail} />
        </pre>
      </details>
    </>
  );
}

function ItemView({ item, call, index, liveThought, force }: { item: Item; call: Call; index: number; liveThought: boolean; force: boolean }) {
  if (item.t === "thought") return <Think text={item.text} live={liveThought} force={force} />;
  if (item.t === "life") return <p className="flow-life">{item.text}</p>;
  if (item.t === "tool") return <Step item={item} seconds={stepSeconds(call, index)} force={force} />;
  return (
    <div className="flow-said">
      <Said text={item.text} done={!!call.end} />
      {!call.end && <span className="caret" aria-hidden />}
    </div>
  );
}

function PlanBlock({ plan }: { plan: Plan }) {
  return (
    <div className="flow-plan" data-testid="locked-plan">
      <div className="flow-kicker">Plan</div>
      {plan.pages.map((p) => (
        <div key={p.id} className="flow-plan-row">
          <span className="mono">{p.id}</span>
          <span className="muted">{p.route}</span>
          <span className="mono muted clip" title={p.owned_glob}>
            {p.owned_glob}
          </span>
        </div>
      ))}
      {plan.estimate.tokens > 0 && <p className="flow-life">About {tokens(plan.estimate.tokens)} tokens.</p>}
      {plan.design?.trim() && <pre className="flow-design">{plan.design.trim()}</pre>}
    </div>
  );
}

function byline(surface: Surface, detail: RunDetail): string {
  if (surface.tag?.harness) return [surface.tag.harness, surface.tag.model].filter(Boolean).join(" · ");
  const role = surface.page ? (detail.pages.find((p) => p.id === surface.page)?.tier ?? "worker") : surface.role;
  const key = (["worker", "mid", "master", "reviewer", "verifier"].includes(role) ? role : "master") as "worker" | "mid" | "master" | "reviewer" | "verifier";
  const harness = detail.roles?.[key];
  const model = detail.models?.[key];
  return [harness, model].filter(Boolean).join(" · ");
}

function agentOf(approval: ToolApproval): string {
  const a = approval.agent;
  if (typeof a === "string") return a;
  if (a && typeof a === "object" && "id" in a && typeof (a as { id: unknown }).id === "string") return (a as { id: string }).id;
  return "";
}

function ownerOf(approval: ToolApproval, list: Surface[]): string {
  const id = agentOf(approval);
  if (list.some((s) => s.id === id)) return id;
  const page = id.startsWith("page:") ? id.slice(5).split("/")[0] : "";
  if (page && list.some((s) => s.id === `page:${page}`)) return `page:${page}`;
  return "master";
}

function ToolAsk({ approval, onAnswer }: { approval: ToolApproval; onAnswer: (id: string, allow: boolean) => void }) {
  const [now, setNow] = useState(() => Date.now());
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    const t = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(t);
  }, []);
  const left = Math.max(0, Math.round((Date.parse(approval.at) + approval.timeout_secs * 1000 - now) / 1000));
  const act = (allow: boolean) => {
    setBusy(true);
    void Promise.resolve(onAnswer(approval.id, allow)).finally(() => setBusy(false));
  };
  return (
    <div className="flow-ask" data-testid="tool-approval">
      <p className="flow-ask-line">
        <span className="mono">{approval.tool}</span>
        <span className="flow-state">{approval.kind}</span>
        {Number.isFinite(left) && approval.timeout_secs > 0 && <span className="flow-by">denies in {left}s</span>}
      </p>
      {approval.command && <pre className="step-io">{approval.command}</pre>}
      {approval.paths.length > 0 && <p className="flow-life">{approval.paths.join("  ")}</p>}
      <p className="flow-said">{approval.reason}</p>
      <div className="flow-actions">
        <button type="button" className="small" data-testid="tool-deny" disabled={busy} onClick={() => act(false)}>
          Deny
        </button>
        <button type="button" className="primary small" data-testid="tool-approve" disabled={busy} onClick={() => act(true)}>
          Approve
        </button>
      </div>
    </div>
  );
}

function AgentBlock({
  surface,
  detail,
  focused,
  force,
  plan,
  ask,
}: {
  surface: Surface;
  detail: RunDetail;
  focused: boolean;
  force: boolean;
  plan?: ReactNode;
  ask?: ReactNode;
}) {
  const master = surface.id === "master";
  const thoughts: { item: Extract<Item, { t: "thought" }>; live: boolean; key: string }[] = [];
  const rest: { item: Item; call: Call; index: number; key: string }[] = [];
  surface.calls.forEach((call, ci) => {
    const last = call.items.length - 1;
    call.items.forEach((item, index) => {
      const live = !call.end && index === last && item.t === "thought";
      if (master && item.t === "thought") thoughts.push({ item, live, key: `${ci}:${index}` });
      else rest.push({ item, call, index, key: `${ci}:${index}` });
    });
    if (call.end?.error) rest.push({ item: { t: "life", text: call.end.error, at: call.end.at }, call, index: -1, key: `${ci}:err` });
  });
  const planning = master && detail.phase === "planning" && surface.state !== "done" && surface.state !== "failed";
  return (
    <section
      className={`flow-agent ${focused ? "focused" : ""}`}
      id={`agent-${surface.id}`}
      data-agent={surface.id}
      data-state={statusWord(surface, detail.phase)}
      tabIndex={-1}
    >
      <header className="flow-role">
        <h2>{surface.title}</h2>
        <span className="flow-state">{statusWord(surface, detail.phase)}</span>
        <span className="flow-by">{byline(surface, detail)}</span>
      </header>
      <div data-agent-text={surface.id}>
        {master &&
          thoughts.map((t) => <Think key={t.key} text={t.item.text} live={t.live} force={force} />)}
        {master && plan}
        {planning && thoughts.length === 0 && rest.length === 0 && (
          <div className="flow-skel-lines" aria-hidden>
            <span />
            <span />
          </div>
        )}
        {rest.map((row) =>
          row.item.t === "thought" ? (
            <Think key={row.key} text={row.item.text} live={!row.call.end && row.index === row.call.items.length - 1} force={force} />
          ) : (
            <ItemView key={row.key} item={row.item} call={row.call} index={row.index} liveThought={false} force={force} />
          ),
        )}
        {surface.calls.length === 0 && !planning && surface.state === "waiting" && <p className="flow-life">Waiting.</p>}
      </div>
      {ask}
    </section>
  );
}

export function FlowSkeleton() {
  return (
    <div className="flow" aria-busy="true" aria-label="Opening the run">
      {[0, 1, 2].map((i) => (
        <div key={i} className="flow-agent">
          <div className="skel skel-role" />
          <div className="skel skel-line" />
          <div className="skel skel-line short" />
        </div>
      ))}
    </div>
  );
}

export function Flow({
  detail,
  list,
  focus,
  blown,
  ask,
  approvals,
  onTool,
}: {
  detail: RunDetail;
  list: Surface[];
  focus: string | null;
  /** Agents whose thinking and steps are held open. */
  blown: ReadonlySet<string>;
  /** Plan approval, rendered under the master. */
  ask?: ReactNode;
  approvals?: ToolApproval[];
  onTool?: (id: string, allow: boolean) => void;
}) {
  const prompt = detail.prompt?.trim();
  const pending = approvals ?? [];
  const settled = detail.phase === "done" || detail.phase === "failed" || detail.cancelled;
  const shown = list.filter((s) => {
    if (s.calls.length > 0 || s.id === "master") return true;
    if (s.role === "worker" && !settled) return true;
    if (s.id === "reviewer" && detail.phase === "reviewing") return true;
    if (s.id === "verifier" && detail.phase === "verifying") return true;
    return false;
  });
  return (
    <div className="flow" data-testid="run-flow" data-phase={detail.phase} role="log" aria-label="Run">
      {prompt && (
        <section className={`flow-agent ${focus === "you" ? "focused" : ""}`} id="agent-you" data-agent="you" tabIndex={-1}>
          <header className="flow-role">
            <h2>You</h2>
          </header>
          <div className="flow-said">
            <Rich text={prompt} />
          </div>
        </section>
      )}
      {shown.map((surface) => {
        const mine = onTool ? pending.filter((a) => ownerOf(a, list) === surface.id) : [];
        return (
          <AgentBlock
            key={surface.id}
            surface={surface}
            detail={detail}
            focused={focus === surface.id}
            force={blown.has(surface.id)}
            plan={surface.id === "master" && detail.plan ? <PlanBlock plan={detail.plan} /> : undefined}
            ask={
              <>
                {surface.id === "master" ? ask : null}
                {mine.map((a) => (
                  <ToolAsk key={a.id} approval={a} onAnswer={onTool!} />
                ))}
              </>
            }
          />
        );
      })}
      {!prompt && list.every((s) => s.calls.length === 0) && <p className="flow-empty">This run has not started.</p>}
    </div>
  );
}
