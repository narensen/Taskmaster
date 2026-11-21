// T3 Code-inspired Run Flow: crisp live headlines, glimpses of reasoning (never raw response
// walls), click-to-inspect activity & accessed files for Master and every Worker, and a clean
// Overall Plan card under Master showing worker delegation and phases.

import { memo, useEffect, useMemo, useState, type ReactNode } from "react";
import {
  AlertCircle,
  Check,
  ChevronRight,
  Circle,
  FileCode2,
  FileText,
  Globe,
  Layers,
  Loader2,
  Pencil,
  Search,
  Sparkles,
  SquareTerminal,
} from "lucide-react";
import type { Phase, Plan, RunDetail, ToolApproval } from "../api";
import { tokens, usd } from "../format";
import { KnownPath, Rich, looksLikePath } from "../paths";
import { unitPrefix, type Call, type Item, type Surface, type ToolItem } from "./model";

const VERB: [RegExp, string, "read" | "edit" | "run" | "search" | "look"][] = [
  [/edit|write|patch|strreplace|apply_patch|create_file|replace/, "Edit", "edit"],
  [/bash|shell|command|exec|terminal|run_code/, "Run", "run"],
  [/search|grep|find|rg\b|glob/, "Search", "search"],
  [/browser|preview|screenshot|navigate/, "Look", "look"],
  [/read|cat\b|open|view|list/, "Read", "read"],
];

function verbOf(name: string): { label: string; kind: "read" | "edit" | "run" | "search" | "look" } {
  const n = name.toLowerCase();
  for (const [re, label, kind] of VERB) if (re.test(n)) return { label, kind };
  return { label: name, kind: "read" };
}

function Glyph({ kind }: { kind: "read" | "edit" | "run" | "search" | "look" }) {
  const Icon =
    kind === "edit" ? Pencil : kind === "run" ? SquareTerminal : kind === "search" ? Search : kind === "look" ? Globe : FileText;
  return <Icon size={13} strokeWidth={1.6} aria-hidden />;
}

/** The first line of a thought, short enough to stand in for the block once it is done. */
export function thinkTitle(text: string): string {
  const line = text
    .trim()
    .split("\n")
    .map((s) => s.trim())
    .find(Boolean);
  if (!line) return "Reasoning";
  return line.length > 96 ? `${line.slice(0, 95).trimEnd()}…` : line;
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
  if (surface.summary.editedFiles.length) return surface.summary.editedFiles[surface.summary.editedFiles.length - 1]!;
  if (surface.summary.readFiles.length) return surface.summary.readFiles[surface.summary.readFiles.length - 1]!;
  for (let c = surface.calls.length - 1; c >= 0; c--) {
    const call = surface.calls[c];
    if (!call) continue;
    for (let i = call.items.length - 1; i >= 0; i--) {
      const item = call.items[i];
      if (item?.t === "tool" && item.target && looksLikePath(item.target.split(/\s+/)[0] ?? "")) {
        return item.target.split(/\s+/)[0]!;
      }
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

function StatusGlyph({ state }: { state: string }) {
  if (state === "working" || state === "planning") {
    return <Loader2 size={14} strokeWidth={1.8} className="spin-ico agent-ico working" aria-hidden />;
  }
  if (state === "done") {
    return <Check size={14} strokeWidth={2} className="agent-ico done" aria-hidden />;
  }
  if (state === "failed") {
    return <AlertCircle size={14} strokeWidth={1.8} className="agent-ico failed" aria-hidden />;
  }
  if (state === "asking") {
    return <Sparkles size={14} strokeWidth={1.7} className="agent-ico asking" aria-hidden />;
  }
  return <Circle size={11} strokeWidth={1.8} className="agent-ico waiting" aria-hidden />;
}

/** How long the agent's open call has run, ticking each second, as `2:05`. */
const Since = memo(function Since({ at }: { at: string }) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, []);
  const secs = Math.max(0, Math.floor((now - Date.parse(at)) / 1000));
  const text =
    secs >= 3600
      ? `${Math.floor(secs / 3600)}:${String(Math.floor((secs % 3600) / 60)).padStart(2, "0")}:${String(secs % 60).padStart(2, "0")}`
      : `${Math.floor(secs / 60)}:${String(secs % 60).padStart(2, "0")}`;
  return (
    <span className="agent-since mono" aria-label={`running for ${text}`}>
      {text}
    </span>
  );
});

function formatDuration(start: string | null, end: string | null): string | null {
  if (!start || !end) return null;
  const ms = Date.parse(end) - Date.parse(start);
  if (!Number.isFinite(ms) || ms < 0) return null;
  const secs = Math.round(ms / 1000);
  if (secs < 60) return `${secs}s`;
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  return `${m}m ${String(s).padStart(2, "0")}s`;
}

/** While the master writes its plan, how many units the reply has laid out so far. */
function planSoFar(call: Call | undefined): number {
  if (!call) return 0;
  const text = call.items.map((item) => (item.t === "text" ? item.text : "")).join("");
  return (text.match(/"owned_glob"\s*:/g) ?? []).length;
}

const Step = memo(function Step({ item, seconds, force }: { item: ToolItem; seconds: string | null; force: boolean }) {
  const [open, setOpen] = useState(false);
  const shown = force || open;
  const { label, kind } = verbOf(item.name);
  const target = item.target ?? "";
  const head = target.split(/\s+/)[0] ?? "";
  const hasBody = item.input !== undefined || !!item.output || !!item.diff;
  const input =
    shown && item.input !== undefined
      ? typeof item.input === "string"
        ? item.input
        : JSON.stringify(item.input, null, 2)
      : "";
  return (
    <div className={`step ${item.allowed === false ? "denied" : ""}`}>
      <button
        type="button"
        className="step-line"
        aria-expanded={shown}
        disabled={!hasBody}
        onClick={() => hasBody && setOpen((v) => !v)}
      >
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
});

const ThoughtNote = memo(function ThoughtNote({ text, live }: { text: string; live: boolean }) {
  const [open, setOpen] = useState(false);
  return (
    <div className={`agent-thought-row ${open ? "open" : ""} ${live ? "live" : ""}`}>
      <button type="button" className="agent-thought-toggle" aria-expanded={open} onClick={() => setOpen((v) => !v)}>
        <ChevronRight size={12} className={`agent-chev ${open ? "open" : ""}`} aria-hidden />
        <span className="clip">{thinkTitle(text)}</span>
      </button>
      {open && (
        <div className="agent-thought-body">
          <Rich text={text} />
          {live && <span className="caret" aria-hidden />}
        </div>
      )}
    </div>
  );
});

/** Rows an expanded agent draws before older ones fold behind a button. */
const ROWS_SHOWN = 80;

type PhaseState = "done" | "checking" | "building" | "planning" | "waiting";

/** Where each phase of the sketch stands, from the run's position in it. */
function phaseStates(detail: RunDetail): PhaseState[] {
  const phases = detail.plan?.phases ?? [];
  const at = detail.phase_at ?? 0;
  const finished = detail.phase === "done";
  return phases.map((_, i) => {
    if (i < at || (i === at && finished)) return "done";
    if (i === at) return detail.phase === "verifying" ? "checking" : "building";
    if (detail.next_phase === i) return "planning";
    return "waiting";
  });
}

/** The sketch as a vertical list: each phase with its state, the active one open with its units. */
const PhaseList = memo(function PhaseList({ detail }: { detail: RunDetail }) {
  const phases = detail.plan?.phases ?? [];
  if (!phases.length) return null;
  const states = phaseStates(detail);
  const pages = detail.plan?.pages ?? [];
  return (
    <ol className="flow-phases" data-testid="phases" aria-label="Phases">
      {phases.map((phase, i) => {
        const open = states[i] === "building" || states[i] === "checking";
        const units = pages.filter((p) => (p.phase ?? 0) === i);
        return (
          <li key={phase.id} className={`flow-phase ${states[i]}`} data-state={states[i]}>
            <div className="flow-phase-line">
              <span className="flow-phase-mark" aria-hidden>
                {states[i] === "done" ? "✓" : open ? "●" : "○"}
              </span>
              <span className="mono">{phase.id}</span>
              <span className="flow-state">{states[i]}</span>
            </div>
            <p className="flow-life">{phase.goal}</p>
            {open && units.length > 0 && <p className="flow-life mono">{units.map((u) => u.id).join(" · ")}</p>}
            {open && phase.test && <p className="flow-life mono">$ {phase.test}</p>}
          </li>
        );
      })}
    </ol>
  );
});

interface PlannedWorkerRow {
  agentId: string;
  label: string;
  route: string;
  glob: string;
  brief: string;
  context?: string;
  done?: string;
  state: string;
}

const OverallPlan = memo(function OverallPlan({
  plan,
  detail,
  surfaces,
  onJump,
}: {
  plan: Plan;
  detail: RunDetail;
  surfaces: Surface[];
  onJump?: (agentId: string) => void;
}) {
  const surfaceById = useMemo(() => {
    const map = new Map<string, Surface>();
    for (const s of surfaces) map.set(s.id, s);
    return map;
  }, [surfaces]);

  const workers: PlannedWorkerRow[] = useMemo(() => {
    return plan.pages.flatMap((page) => {
      const prefix = unitPrefix(plan, page);
      const pageState = detail.pages.find((p) => p.id === page.id)?.state;
      const parts = page.units && page.units.length >= 2 && page.units.length <= 16 ? page.units : null;
      if (parts) {
        return parts.map((u) => {
          const agentId = `${prefix}:${page.id}/${u.id}`;
          const s = surfaceById.get(agentId);
          return {
            agentId,
            label: `${page.id} / ${u.id}`,
            route: page.route,
            glob: u.owned_glob,
            brief: u.brief.trim(),
            context: u.context?.trim() || page.context?.trim(),
            done: u.done?.trim() || page.done?.trim(),
            state: s ? s.state : pageState === "passed" ? "done" : pageState === "failed" ? "failed" : "waiting",
          };
        });
      }
      const agentId = `${prefix}:${page.id}`;
      const s = surfaceById.get(agentId);
      return [
        {
          agentId,
          label: page.id,
          route: page.route,
          glob: page.owned_glob,
          brief: page.brief.trim(),
          context: page.context?.trim(),
          done: page.done?.trim(),
          state: s ? s.state : pageState === "passed" ? "done" : pageState === "failed" ? "failed" : "waiting",
        },
      ];
    });
  }, [plan, detail.pages, surfaceById]);

  const [expandedWorker, setExpandedWorker] = useState<string | null>(null);
  const parallelCap = detail.parallel ?? 0;

  return (
    <div className="plan-card" data-testid="locked-plan">
      <div className="plan-card-head">
        <span className="plan-card-title">
          <Layers size={13} strokeWidth={1.7} aria-hidden />
          Overall plan
        </span>
        <span className="plan-card-meta">
          {plan.kind && <span className="plan-pill mono">{plan.kind}</span>}
          <span className="plan-pill">
            {workers.length} worker{workers.length === 1 ? "" : "s"}
            {parallelCap > 1 ? ` · up to ${Math.min(parallelCap, Math.max(workers.length, 1))} parallel` : ""}
          </span>
          {plan.contracts.length > 0 && (
            <span className="plan-pill">
              {plan.contracts.length} contract{plan.contracts.length === 1 ? "" : "s"}
            </span>
          )}
          {plan.estimate.tokens > 0 && (
            <span className="plan-pill mono">
              ~{tokens(plan.estimate.tokens)} tok{plan.estimate.priced ? ` · ${usd(plan.estimate.usd)}` : ""}
            </span>
          )}
        </span>
      </div>

      {plan.summary?.trim() && <p className="plan-summary">{plan.summary.trim()}</p>}

      <div className="plan-workers" data-testid="plan-workers">
        {workers.map((w) => {
          const open = expandedWorker === w.agentId;
          const briefLine = w.brief.split("\n")[0] ?? "";
          return (
            <div key={w.agentId} className={`plan-worker ${w.state}`}>
              <div className="plan-worker-row">
                <button
                  type="button"
                  className="plan-worker-btn"
                  aria-expanded={open}
                  onClick={() => setExpandedWorker(open ? null : w.agentId)}
                >
                  <StatusGlyph state={w.state} />
                  <span className="plan-worker-id mono">{w.label}</span>
                  {w.route && <span className="plan-worker-route muted">{w.route}</span>}
                  <span className="plan-worker-glob mono muted clip" title={w.glob}>
                    {w.glob}
                  </span>
                </button>
                {onJump && (
                  <button
                    type="button"
                    className="plan-jump"
                    title={`Jump to ${w.label}`}
                    onClick={() => onJump(w.agentId)}
                  >
                    Inspect
                  </button>
                )}
              </div>
              {briefLine && !open && <p className="plan-worker-brief clip">{briefLine}</p>}
              {open && (
                <div className="plan-worker-detail">
                  {w.brief && (
                    <p className="plan-worker-full">
                      <Rich text={w.brief} />
                    </p>
                  )}
                  {w.context && (
                    <div className="plan-worker-ctx">
                      <span className="plan-sublabel">Context</span>
                      <p>
                        <Rich text={w.context} />
                      </p>
                    </div>
                  )}
                  {w.done && (
                    <div className="plan-worker-ctx">
                      <span className="plan-sublabel">Done when</span>
                      <p>{w.done}</p>
                    </div>
                  )}
                </div>
              )}
            </div>
          );
        })}
      </div>

      <PhaseList detail={detail} />

      {plan.contracts.length > 0 && (
        <details className="plan-fold">
          <summary>
            Shared contracts ({plan.contracts.length})
          </summary>
          <div className="plan-contracts">
            {plan.contracts.map((c) => (
              <div key={c.path} className="plan-contract">
                <div className="plan-contract-path mono">
                  <KnownPath path={c.path} />
                </div>
                {c.content.trim() && <pre className="step-io">{c.content.trim()}</pre>}
              </div>
            ))}
          </div>
        </details>
      )}

      {plan.design?.trim() && (
        <details className="plan-fold">
          <summary>Design system</summary>
          <pre className="flow-design">{plan.design.trim()}</pre>
        </details>
      )}
    </div>
  );
});

function byline(surface: Surface, detail: RunDetail): string {
  if (surface.tag?.harness) return [surface.tag.harness, surface.tag.model].filter(Boolean).join(" · ");
  const role = surface.page ? (detail.pages.find((p) => p.id === surface.page)?.tier ?? "worker") : surface.role;
  const key = (["worker", "mid", "master", "reviewer", "verifier"].includes(role) ? role : "master") as
    | "worker"
    | "mid"
    | "master"
    | "reviewer"
    | "verifier";
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
  const page = id.startsWith("page:") || id.startsWith("unit:") ? id.slice(5).split("/")[0] : "";
  const whole = page ? list.find((s) => s.id === `page:${page}` || s.id === `unit:${page}`) : undefined;
  if (whole) return whole.id;
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

/** Lazy expanded activity inspector: only computed & mounted when the user clicks the headline. */
const AgentActivity = memo(function AgentActivity({
  surface,
  force,
}: {
  surface: Surface;
  force: boolean;
}) {
  const [showAll, setShowAll] = useState(false);
  const { editedFiles, readFiles, commands } = surface.summary;

  const { activity, rawOutputs } = useMemo(() => {
    const rows: { item: Item; call: Call; index: number; key: string }[] = [];
    const texts: { text: string; key: string }[] = [];
    surface.calls.forEach((call, ci) => {
      call.items.forEach((item, index) => {
        if (item.t === "text") {
          if (item.text.trim()) texts.push({ text: item.text.trim(), key: `${ci}:${index}` });
        } else {
          rows.push({ item, call, index, key: `${ci}:${index}` });
        }
      });
      if (call.end?.error) {
        rows.push({ item: { t: "life", text: call.end.error, at: call.end.at }, call, index: -1, key: `${ci}:err` });
      }
    });
    return { activity: rows, rawOutputs: texts };
  }, [surface.calls]);

  const hidden = showAll ? 0 : Math.max(0, activity.length - ROWS_SHOWN);
  const visible = hidden ? activity.slice(hidden) : activity;
  const hasFiles = editedFiles.length > 0 || readFiles.length > 0;

  return (
    <div className="agent-activity" data-testid="agent-activity">
      {hasFiles && (
        <div className="agent-files" data-testid="agent-files">
          {editedFiles.length > 0 && (
            <div className="agent-file-group">
              <span className="agent-section-label">
                <Pencil size={11} aria-hidden /> Edited ({editedFiles.length})
              </span>
              <div className="agent-file-chips">
                {editedFiles.map((f) => (
                  <span key={f} className="agent-file-chip edited">
                    <KnownPath path={f} />
                  </span>
                ))}
              </div>
            </div>
          )}
          {readFiles.length > 0 && (
            <div className="agent-file-group">
              <span className="agent-section-label">
                <FileCode2 size={11} aria-hidden /> Accessed ({readFiles.length})
              </span>
              <div className="agent-file-chips">
                {readFiles.map((f) => (
                  <span key={f} className="agent-file-chip">
                    <KnownPath path={f} />
                  </span>
                ))}
              </div>
            </div>
          )}
        </div>
      )}

      {commands.length > 0 && (
        <div className="agent-commands">
          <span className="agent-section-label">
            <SquareTerminal size={11} aria-hidden /> Commands ({commands.length})
          </span>
          <div className="agent-cmd-list">
            {commands.slice(-8).map((cmd, i) => (
              <code key={`${i}:${cmd}`} className="agent-cmd-chip">
                $ {cmd}
              </code>
            ))}
          </div>
        </div>
      )}

      {activity.length > 0 ? (
        <div className="agent-steps">
          <span className="agent-section-label">Activity ({activity.length})</span>
          {hidden > 0 && (
            <button type="button" className="link small flow-earlier" onClick={() => setShowAll(true)}>
              Show {hidden} earlier step{hidden === 1 ? "" : "s"}
            </button>
          )}
          {visible.map((row) =>
            row.item.t === "thought" ? (
              <ThoughtNote
                key={row.key}
                text={row.item.text}
                live={!row.call.end && row.index === row.call.items.length - 1}
              />
            ) : row.item.t === "life" ? (
              <p key={row.key} className="flow-life">
                {row.item.text}
              </p>
            ) : row.item.t === "tool" ? (
              <Step
                key={row.key}
                item={row.item}
                seconds={stepSeconds(row.call, row.index)}
                force={force}
              />
            ) : null,
          )}
        </div>
      ) : (
        <p className="flow-life">No tool steps recorded yet.</p>
      )}

      {rawOutputs.length > 0 && (
        <details className="agent-raw">
          <summary>Raw model output ({rawOutputs.length})</summary>
          <div className="agent-raw-body">
            {rawOutputs.map((r) => (
              <pre key={r.key} className="step-io">
                <Rich text={r.text} />
              </pre>
            ))}
          </div>
        </details>
      )}
    </div>
  );
});

const AgentBlock = memo(function AgentBlock({
  surface,
  detail,
  focused,
  expanded,
  onToggle,
  plan,
  ask,
}: {
  surface: Surface;
  detail: RunDetail;
  focused: boolean;
  expanded: boolean;
  onToggle: (id: string) => void;
  plan?: ReactNode;
  ask?: ReactNode;
}) {
  const master = surface.id === "master";
  const state = statusWord(surface, detail.phase);
  const openCall = surface.calls.length ? surface.calls[surface.calls.length - 1] : undefined;
  const running = openCall && !openCall.end ? openCall : undefined;
  const planning = master && detail.phase === "planning" && surface.state !== "done" && surface.state !== "failed";
  const written = planning ? planSoFar(running) : 0;

  const { headline, thoughtGlimpse, editedFiles, readFiles, toolCount, error, latestStart, latestEnd } = surface.summary;
  const totalFiles = editedFiles.length + readFiles.length;
  const previewFiles = editedFiles.length ? editedFiles : readFiles;
  const duration = !running ? formatDuration(latestStart, latestEnd) : null;
  const label = master ? "Master" : surface.title.replace(/^Worker · /, "");

  const displayHeadline =
    planning && written > 0
      ? `Drafting plan · ${written} worker unit${written === 1 ? "" : "s"} defined`
      : headline;

  const counts: string[] = [];
  if (totalFiles > 0) counts.push(`${totalFiles} file${totalFiles === 1 ? "" : "s"}`);
  if (toolCount > 0) counts.push(`${toolCount} step${toolCount === 1 ? "" : "s"}`);
  if (duration) counts.push(duration);

  return (
    <section
      className={`agent-card ${master ? "is-master" : "is-worker"} ${expanded ? "is-open" : ""} ${focused ? "focused" : ""}`}
      id={`agent-${surface.id}`}
      data-agent={surface.id}
      data-state={state}
      tabIndex={-1}
    >
      <button
        type="button"
        className="agent-headline-bar"
        data-testid="agent-headline"
        aria-expanded={expanded}
        onClick={() => onToggle(surface.id)}
      >
        <StatusGlyph state={state} />
        <span className="agent-name">{label}</span>
        <span className="agent-headline-text clip" title={displayHeadline}>
          {displayHeadline}
        </span>
        <span className="agent-meta">
          {counts.length > 0 && <span className="agent-stat mono">{counts.join(" · ")}</span>}
          {running && <Since at={running.at} />}
          <span className="agent-by clip">{byline(surface, detail)}</span>
          <ChevronRight size={14} className={`agent-chev ${expanded ? "open" : ""}`} aria-hidden />
        </span>
      </button>

      {thoughtGlimpse && !expanded && (
        <div className={`agent-glimpse ${running ? "live" : ""}`} data-testid="agent-glimpse">
          <span className="agent-glimpse-text">{thoughtGlimpse}</span>
        </div>
      )}

      {!expanded && previewFiles.length > 0 && (
        <div className="agent-file-strip" aria-label="Accessed files">
          {previewFiles.slice(0, 4).map((f) => (
            <span key={f} className={`agent-file-chip ${editedFiles.includes(f) ? "edited" : ""}`}>
              <KnownPath path={f} />
            </span>
          ))}
          {totalFiles > 4 && (
            <button type="button" className="agent-file-more" onClick={() => onToggle(surface.id)}>
              +{totalFiles - 4} more
            </button>
          )}
        </div>
      )}

      {error && !expanded && (
        <p className="agent-error-line">
          <Rich text={error} />
        </p>
      )}

      {expanded && <AgentActivity surface={surface} force={false} />}

      {master && plan}
      {ask}
    </section>
  );
});

export function FlowSkeleton() {
  return (
    <div className="flow" aria-busy="true" aria-label="Opening the run">
      {[0, 1, 2].map((i) => (
        <div key={i} className="agent-card">
          <div className="agent-headline-bar">
            <div className="skel skel-role" />
            <div className="skel skel-line short" />
          </div>
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
  onToggleAgent,
  ask,
  approvals,
  onTool,
  onFocusAgent,
}: {
  detail: RunDetail;
  list: Surface[];
  focus: string | null;
  /** Agents whose activity drawers are open. */
  blown: ReadonlySet<string>;
  onToggleAgent: (agentId: string) => void;
  /** Plan approval, rendered under the master. */
  ask?: ReactNode;
  approvals?: ToolApproval[];
  onTool?: (id: string, allow: boolean) => void;
  onFocusAgent?: (agentId: string) => void;
}) {
  const prompt = detail.prompt?.trim();
  const pending = approvals ?? [];
  const shown = useMemo(
    () =>
      list.filter((s) => {
        if (s.calls.length > 0 || s.id === "master" || s.role === "worker") return true;
        if (s.id === "reviewer" && detail.phase === "reviewing") return true;
        if (s.id === "verifier" && detail.phase === "verifying") return true;
        return false;
      }),
    [list, detail.phase],
  );

  const master = shown.find((s) => s.id === "master");
  const workers = shown.filter((s) => s.role === "worker");
  const others = shown.filter((s) => s.id !== "master" && s.role !== "worker");
  const activeWorkers = workers.filter((w) => w.state === "working").length;
  const doneWorkers = workers.filter((w) => w.state === "done").length;

  const renderAgent = (surface: Surface) => {
    const mine = onTool ? pending.filter((a) => ownerOf(a, list) === surface.id) : [];
    const isMaster = surface.id === "master";
    const hasAsk = (isMaster && !!ask) || mine.length > 0;
    return (
      <AgentBlock
        key={surface.id}
        surface={surface}
        detail={detail}
        focused={focus === surface.id}
        expanded={blown.has(surface.id)}
        onToggle={onToggleAgent}
        plan={
          isMaster && detail.plan ? (
            <OverallPlan plan={detail.plan} detail={detail} surfaces={list} onJump={onFocusAgent} />
          ) : undefined
        }
        ask={
          hasAsk ? (
            <>
              {isMaster && ask}
              {mine.map((a) => (
                <ToolAsk key={a.id} approval={a} onAnswer={onTool!} />
              ))}
            </>
          ) : undefined
        }
      />
    );
  };

  return (
    <div className="flow" data-testid="run-flow" data-phase={detail.phase} role="log" aria-label="Run">
      {prompt && (
        <section
          className={`user-prompt ${focus === "you" ? "focused" : ""}`}
          id="agent-you"
          data-agent="you"
          tabIndex={-1}
        >
          <div className="user-prompt-body" data-agent-text="you">
            <Rich text={prompt} />
          </div>
        </section>
      )}

      {master && renderAgent(master)}

      {workers.length > 0 && (
        <div className="workers-group" data-testid="workers-group">
          <div className="workers-group-head">
            <span>
              Workers ({doneWorkers}/{workers.length} done{activeWorkers > 0 ? ` · ${activeWorkers} active` : ""})
            </span>
            {detail.parallel && detail.parallel > 1 && (
              <span className="mono muted">pool: {detail.parallel} parallel</span>
            )}
          </div>
          <div className="workers-stack">{workers.map(renderAgent)}</div>
        </div>
      )}

      {others.map(renderAgent)}

      {!prompt && list.every((s) => s.calls.length === 0) && <p className="flow-empty">This run has not started.</p>}
    </div>
  );
}
