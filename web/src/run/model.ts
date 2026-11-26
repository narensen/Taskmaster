// A run as the control surface sees it: its log, its agents' transcript, its notes and its
// ledger, followed live over the daemon's stream.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  api,
  follow,
  type AgentEntry,
  type AgentTag,
  type LedgerEntry,
  type OlderCallSummary,
  type PagePlan,
  type PageState,
  type Plan,
  type Note,
  type RunDetail,
  type RunEvent,
  type RunMode,
  type Totals,
} from "../api";
import { keys, queryClient } from "../query";
import { checksFailed, pageDone } from "../notify";
import { looksLikePath } from "../paths";

export interface ToolItem {
  t: "tool";
  /** The harness's id for this call, when it sent one. */
  id?: string;
  name: string;
  target: string | null;
  allowed?: boolean;
  input?: unknown;
  /** Text the harness printed for this call, when it sent any. */
  output?: string;
  /** A diff the harness attached. Absent when it sent none. */
  diff?: string;
  /** P5: oversized output body omitted on wire and fetched on demand via GET /api/runs/{id}/agents/{bodyIndex}. */
  bodyOmitted?: boolean;
  bodyIndex?: number;
  outputBytes?: number;
  diffBytes?: number;
  at: string;
}
/** A lifecycle line: the agent stalled and was replaced, or was released when done. */
export type Item =
  | { t: "text"; text: string; at: string }
  | { t: "thought"; text: string; at: string }
  | { t: "life"; text: string; at: string }
  | ToolItem;

export interface Call {
  call: number;
  tag: AgentTag;
  items: Item[];
  brief?: string;
  end?: AgentEntry;
  at: string;
}

export interface SurfaceSummary {
  /** Single crisp headline describing what this agent is doing right now or what it accomplished. */
  headline: string;
  /** Latest concise glimpse of reasoning (only present when the harness emitted thoughts). */
  thoughtGlimpse: string | null;
  /** Deduplicated files created or modified by this agent. */
  editedFiles: string[];
  /** Deduplicated files read/inspected by this agent (excluding editedFiles). */
  readFiles: string[];
  /** Shell commands executed by this agent. */
  commands: string[];
  /** Total number of tool steps taken. */
  toolCount: number;
  /** Latest error message from a failed call, if any. */
  error: string | null;
  /** Timestamp of the latest running or completed call. */
  latestStart: string | null;
  latestEnd: string | null;
}

export interface Surface {
  id: string;
  title: string;
  role: string;
  page: string | null;
  calls: Call[];
  tag: AgentTag | null;
  /** `stopped`: a call was still open when the run was cancelled or ended. */
  state: "waiting" | "working" | "done" | "failed" | "stopped";
  /** Latest explicit status set by the agent via status(text) / set_status (O6). */
  statusLine?: string;
  summary: SurfaceSummary;
  tokens: number;
  /** The same tokens, split four ways, from each finished call's usage. */
  split: TokenSplit;
  usage: boolean;
  cost: number;
  calls_billed: number;
}

export interface TokenSplit {
  input: number;
  output: number;
  cache_read: number;
  cache_write: number;
}

/** A later chunk replaces a result it already contains. Otherwise the new text follows. */
function mergeChunk(prev: string | undefined, next: string): string {
  if (!prev) return next;
  if (next.startsWith(prev) || prev.endsWith(next)) return next.startsWith(prev) ? next : prev;
  return prev + next;
}

/** The one unit of an Agent-mode run or follow-up turn: `agent`, or `agent-<turn>`. */
export function isAgentUnit(page: string | null | undefined): boolean {
  return !!page && /^agent(-\d+)?$/.test(page);
}

/** The page or unit an agent id names (`page:a`, `unit:a/part`), or null for any other agent. */
export function workerPart(id: string): string | null {
  const hit = /^(?:page|unit):(.+)$/.exec(id);
  return hit ? hit[1]! : null;
}

/** The prefix a unit's agents run under: `page` when a browser shows it, `unit` otherwise. */
export function unitPrefix(plan: Plan | null | undefined, page: PagePlan | undefined): "page" | "unit" {
  const web = (plan?.kind ?? "web") === "web";
  return (page?.ui ?? web) ? "page" : "unit";
}

export function surfaceOf(entry: LedgerEntry): string {
  if (["planner", "master", "arbiter"].includes(entry.role)) return "master";
  if ((entry.role === "worker" || entry.role === "mid") && entry.page) return entry.agent && workerPart(entry.agent) ? entry.agent : `page:${entry.page}`;
  if (entry.role === "security" || entry.role === "reviewer") return "reviewer";
  return entry.role;
}

/** Entry kinds that belong to an agent call. */
const CALL_KINDS = new Set<string>(["start", "text", "thought", "tool_call", "command", "output", "tool_diff", "mcp_reply", "preview", "permission", "end", "stalled", "released", "restarted"]);

const EDIT_TOOL_RE = /edit|write|patch|strreplace|apply_patch|create_file|replace/i;
const RUN_TOOL_RE = /bash|shell|command|exec|terminal|run_code/i;
const SEARCH_TOOL_RE = /search|grep|find|rg\b|glob/i;
const LOOK_TOOL_RE = /browser|preview|screenshot|navigate/i;
const READ_TOOL_RE = /read|cat\b|open|view|list/i;

function shortName(path: string): string {
  const parts = path.split("/").filter(Boolean);
  if (parts.length <= 2) return path;
  return parts.slice(-2).join("/");
}

function cleanLine(text: string, max = 140): string {
  const cleaned = text
    .replace(/\r/g, "")
    .split("\n")
    .map((l) => l.trim().replace(/^[#>*`-\s]+/, "").trim())
    .find((l) => l.length > 0 && !l.startsWith("{") && !l.startsWith("```"));
  if (!cleaned) return "";
  return cleaned.length > max ? `${cleaned.slice(0, max - 1).trimEnd()}…` : cleaned;
}

function extractGlimpse(text: string, max = 180): string | null {
  const lines = text
    .replace(/\r/g, "")
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l.length > 0 && !l.startsWith("```"));
  if (!lines.length) return null;
  // Take the latest meaningful sentence/line so streaming thoughts feel alive.
  const picked = lines.length >= 2 ? `${lines[lines.length - 2]} ${lines[lines.length - 1]}` : lines[0]!;
  const collapsed = picked.replace(/\s+/g, " ").trim();
  if (!collapsed) return null;
  return collapsed.length > max ? `${collapsed.slice(0, max - 1).trimEnd()}…` : collapsed;
}

function toolHeadline(item: ToolItem): string {
  const target = (item.target ?? "").trim();
  const firstToken = target.split(/\s+/)[0] ?? "";
  const isPath = firstToken && looksLikePath(firstToken);
  const name = item.name;
  if (EDIT_TOOL_RE.test(name)) return isPath ? `Editing ${shortName(firstToken)}` : target ? `Editing ${target.slice(0, 64)}` : "Editing files";
  if (RUN_TOOL_RE.test(name)) return target ? `Running ${target.slice(0, 68)}` : "Running command";
  if (SEARCH_TOOL_RE.test(name)) return target ? `Searching ${target.slice(0, 64)}` : "Searching codebase";
  if (LOOK_TOOL_RE.test(name)) return target ? `Checking ${target.slice(0, 64)}` : "Checking browser preview";
  if (READ_TOOL_RE.test(name)) return isPath ? `Reading ${shortName(firstToken)}` : target ? `Reading ${target.slice(0, 64)}` : "Reading files";
  return target ? `${name} ${target.slice(0, 60)}` : name;
}

function computeSummary(s: Surface, detail: RunDetail | null): SurfaceSummary {
  const editedSet = new Set<string>();
  const readSet = new Set<string>();
  const commands: string[] = [];
  let toolCount = 0;
  let lastTool: ToolItem | null = null;
  let latestThought: string | null = null;
  let latestStatus: string | null = null;
  let lastError: string | null = null;
  let latestStart: string | null = null;
  let latestEnd: string | null = null;

  for (const call of s.calls) {
    latestStart = call.at;
    latestEnd = call.end?.at ?? null;
    if (call.end?.error && !call.end.stopped) lastError = call.end.error;
    for (const item of call.items) {
      if (item.t === "thought" && item.text.trim()) {
        latestThought = item.text;
      } else if (item.t === "life" && item.text.startsWith("Status: ")) {
        latestStatus = item.text.slice("Status: ".length).trim() || latestStatus;
      } else if (item.t === "tool") {
        toolCount++;
        lastTool = item;
        const raw = (item.target ?? "").trim();
        const firstToken = raw.split(/\s+/)[0] ?? "";
        if (RUN_TOOL_RE.test(item.name)) {
          if (raw && commands[commands.length - 1] !== raw) commands.push(raw);
        } else if (firstToken && looksLikePath(firstToken)) {
          if (EDIT_TOOL_RE.test(item.name)) {
            editedSet.add(firstToken);
            readSet.delete(firstToken);
          } else if (!editedSet.has(firstToken)) {
            readSet.add(firstToken);
          }
        }
      }
    }
  }
  if (s.statusLine) latestStatus = s.statusLine;

  const editedFiles = [...editedSet];
  const readFiles = [...readSet];
  const thoughtGlimpse = latestThought ? extractGlimpse(latestThought) : null;
  const isMaster = s.id === "master";
  const open = s.calls.length > 0 && !s.calls[s.calls.length - 1]!.end;

  let headline = "Waiting";
  if (s.state === "stopped") {
    headline = lastTool ? `Stopped while ${toolHeadline(lastTool).replace(/^\w/, (c) => c.toLowerCase())}` : "Stopped";
  } else if (s.state === "failed" || lastError) {
    headline = lastError ? cleanLine(lastError, 96) || "Failed" : "Failed";
  } else if (latestStatus && (open || s.state === "working" || s.state === "done")) {
    headline = cleanLine(latestStatus, 120) || latestStatus;
  } else if (open) {
    if (lastTool) {
      headline = toolHeadline(lastTool);
    } else if (thoughtGlimpse) {
      headline = cleanLine(thoughtGlimpse, 92) || (isMaster ? "Planning architecture & workers" : "Reasoning through task");
    } else if (isMaster && detail?.phase === "planning") {
      headline = "Planning architecture & worker split";
    } else if (s.id === "reviewer") {
      headline = "Reviewing changes";
    } else if (s.id === "verifier") {
      headline = "Verifying findings";
    } else {
      headline = "Working on assigned unit";
    }
  } else if (s.state === "done" || (isMaster && detail?.plan)) {
    if (isMaster && detail?.plan) {
      const workerCount = detail.plan.pages.reduce((n, p) => n + (p.units && p.units.length > 1 ? p.units.length : 1), 0);
      if (detail.phase === "awaiting_approval") {
        headline = `Plan ready · ${workerCount} worker${workerCount === 1 ? "" : "s"} awaiting approval`;
      } else {
        headline = `Planned ${workerCount} worker${workerCount === 1 ? "" : "s"}${detail.plan.contracts.length ? ` · ${detail.plan.contracts.length} contract${detail.plan.contracts.length === 1 ? "" : "s"}` : ""}`;
      }
    } else {
      const parts: string[] = [];
      if (editedFiles.length) parts.push(`Edited ${editedFiles.length} file${editedFiles.length === 1 ? "" : "s"}`);
      if (readFiles.length && !editedFiles.length) parts.push(`Read ${readFiles.length} file${readFiles.length === 1 ? "" : "s"}`);
      if (commands.length) parts.push(`ran ${commands.length} command${commands.length === 1 ? "" : "s"}`);
      headline = parts.length
        ? parts.join(" · ")
        : toolCount
          ? `Completed ${toolCount} step${toolCount === 1 ? "" : "s"}`
          : s.calls.length === 0
            ? "Up to date"
            : "Completed";
    }
  } else if (isMaster && detail?.phase === "planning") {
    headline = "Starting plan";
  } else if (s.role === "worker") {
    headline = "Queued";
  }

  return {
    headline,
    thoughtGlimpse,
    editedFiles,
    readFiles,
    commands,
    toolCount,
    error: lastError,
    latestStart,
    latestEnd,
  };
}

const EMPTY_SUMMARY: SurfaceSummary = {
  headline: "Waiting",
  thoughtGlimpse: null,
  editedFiles: [],
  readFiles: [],
  commands: [],
  toolCount: 0,
  error: null,
  latestStart: null,
  latestEnd: null,
};

function blankSurface(id: string): Surface {
  const raw = workerPart(id);
  const page = raw ? raw.split("/")[0]! : null;
  const role = page ? "worker" : id;
  return {
    id,
    title: raw && isAgentUnit(raw) ? "Agent" : raw ? `Worker · ${raw.replaceAll("/", " / ")}` : id[0]!.toUpperCase() + id.slice(1),
    role,
    page,
    calls: [],
    tag: null,
    state: "waiting",
    summary: EMPTY_SUMMARY,
    tokens: 0,
    split: { input: 0, output: 0, cache_read: 0, cache_write: 0 },
    usage: false,
    cost: 0,
    calls_billed: 0,
  };
}

function findLastTool(items: Item[], match: (t: ToolItem) => boolean): ToolItem | undefined {
  for (let i = items.length - 1; i >= 0; i--) {
    const item = items[i];
    if (item && item.t === "tool" && match(item)) return item;
  }
  return undefined;
}

/**
 * Builds the agents' transcript from entries as they arrive, touching only what changed.
 * A streamed fragment replaces the one item it extends and the call and agent it sits in;
 * every other item, call and agent keeps its identity, so memoised rows do not re-render.
 */
export class SurfaceBuilder {
  private seen = 0;
  /** The first entry this builder reads: a later turn starts part way through the transcript. */
  private from = 0;
  private first: AgentEntry | undefined;
  private last: AgentEntry | undefined;
  private byId = new Map<string, Surface>();
  /** Where each call sits: its agent, and its index in that agent's calls. */
  private where = new Map<string, { id: string; index: number }>();
  /** Full agent tag seen on `start` (or richest tag seen) per `id#call`, for P2 compact entries. */
  private tags = new Map<string, AgentTag>();
  private dirtySummary = new Set<string>();

  /** Read from entry `from` on, for a turn that starts there. */
  start(from: number): this {
    this.from = from;
    this.seen = Math.max(this.seen, from);
    return this;
  }

  /** Whether `entries` is the list this builder has read so far, with more on the end. */
  continues(entries: AgentEntry[], from = 0): boolean {
    if (from !== this.from) return false;
    if (this.seen === this.from) return true;
    return entries.length >= this.seen && entries[this.from] === this.first && entries[this.seen - 1] === this.last;
  }

  /** Read the entries up to `until` (all of them by default) that it has not read yet. */
  apply(entries: AgentEntry[], until = entries.length): this {
    const end = Math.min(until, entries.length);
    if (end <= this.seen) return this;
    const touched = new Set<string>();
    const fresh = new Set<string>();
    const resolveTag = (key: string, incoming: AgentTag): AgentTag => {
      const prev = this.tags.get(key);
      if (!prev) {
        this.tags.set(key, incoming);
        return incoming;
      }
      if (incoming.role || incoming.harness || incoming.model || incoming.page || incoming.unit || incoming.worktree || incoming.owned_glob) {
        const merged = { ...prev, ...incoming };
        this.tags.set(key, merged);
        return merged;
      }
      return prev;
    };
    // A surface and a call are copied once per batch, the first time the batch touches them.
    const surface = (id: string): Surface => {
      let s = this.byId.get(id);
      if (!s) {
        s = blankSurface(id);
        this.byId.set(id, s);
        touched.add(id);
        this.dirtySummary.add(id);
        return s;
      }
      if (!touched.has(id)) {
        s = { ...s, calls: s.calls.slice(), split: { ...s.split } };
        this.byId.set(id, s);
        touched.add(id);
        this.dirtySummary.add(id);
      }
      return s;
    };
    const callOf = (s: Surface, key: string, e: AgentEntry, tag: AgentTag): Call => {
      const at = this.where.get(key);
      if (!at) {
        const call: Call = { call: e.agent.call ?? 0, tag, items: [], at: e.at };
        this.where.set(key, { id: s.id, index: s.calls.length });
        s.calls.push(call);
        fresh.add(key);
        return call;
      }
      let call = s.calls[at.index]!;
      if (!fresh.has(key)) {
        call = { ...call, items: call.items.slice() };
        s.calls[at.index] = call;
        fresh.add(key);
      }
      if (call.tag !== tag) call.tag = tag;
      return call;
    };
    const replace = <T extends Item>(call: Call, item: T, change: Partial<T>): T => {
      const next = { ...item, ...change };
      const i = call.items.lastIndexOf(item);
      if (i >= 0) call.items[i] = next;
      return next;
    };
    for (let n = this.seen; n < end; n++) {
      const e = entries[n]!;
      const id = e.agent.id;
      if (id === "runtime" || id === "person") continue;
      // Run-level lines (cancelling, budget, plan review) carry no call: they are not an agent at work.
      if (e.agent.call == null && !CALL_KINDS.has(e.kind)) continue;
      const key = `${e.agent.id}#${e.agent.call ?? 0}`;
      const tag = resolveTag(key, e.agent);
      const s = surface(id);
      const call = callOf(s, key, e, tag);
      s.tag = tag;
      const last = call.items[call.items.length - 1];
      switch (e.kind) {
        case "start":
          call.brief = e.brief;
          break;
        case "text":
          if (last?.t === "text") replace(call, last, { text: last.text + (e.text ?? "") });
          else call.items.push({ t: "text", text: e.text ?? "", at: e.at });
          break;
        case "thought":
          if (last?.t === "thought") replace(call, last, { text: last.text + (e.text ?? "") });
          else call.items.push({ t: "thought", text: e.text ?? "", at: e.at });
          break;
        case "tool_call":
          call.items.push({ t: "tool", id: e.id, name: e.name ?? "tool", target: e.target ?? null, input: e.input, at: e.at });
          break;
        case "command":
          call.items.push({ t: "tool", name: "command", target: e.command ?? null, at: e.at });
          break;
        case "output":
        case "tool_diff": {
          const byTool = e.id ? findLastTool(call.items, (i) => i.id === e.id) : undefined;
          const open = byTool ?? findLastTool(call.items, (i) => i.output === undefined && i.diff === undefined && !i.bodyOmitted);
          if (open) {
            const change: Partial<ToolItem> = {};
            if (e.text) change.output = mergeChunk(open.output, e.text);
            if (e.diff) change.diff = mergeChunk(open.diff, e.diff);
            if (e.body_omitted) {
              change.bodyOmitted = true;
              if (e.body_index !== undefined) change.bodyIndex = e.body_index;
              if (e.text_bytes !== undefined || e.bytes !== undefined) change.outputBytes = e.text_bytes ?? e.bytes;
              if (e.diff_bytes !== undefined) change.diffBytes = e.diff_bytes;
            }
            replace(call, open, change);
          } else if (e.text) call.items.push({ t: "text", text: e.text, at: e.at });
          break;
        }
        case "preview":
          call.items.push({ t: "tool", name: "browser", target: e.routes?.map((r) => r.route).join(" ") || e.step || null, at: e.at });
          break;
        case "permission": {
          const targetName = e.tool ?? e.name;
          const open = findLastTool(call.items, (i) => i.allowed === undefined && i.name === targetName);
          if (open) replace(call, open, { allowed: e.allowed });
          else call.items.push({ t: "tool", name: e.tool ?? "tool", target: null, allowed: e.allowed, at: e.at });
          break;
        }
        case "status": {
          const text = (e.text ?? "").trim();
          if (text) {
            s.statusLine = text;
            call.items.push({ t: "life", text: `Status: ${text}`, at: e.at });
          }
          break;
        }
        case "stalled":
          call.items.push({ t: "life", text: `Stalled waiting on its model; fresh agent ${e.restart ?? 1} started.`, at: e.at });
          break;
        case "restarted":
          call.items.push({ t: "life", text: "Restarted by user; starting a fresh worker.", at: e.at });
          break;
        case "released": {
          const left = e.lingering ? `${e.lingering} still running` : "nothing left running";
          const count = e.processes ?? 0;
          call.items.push({ t: "life", text: `${e.asked ? "Reported done and asked to be released" : "Turn ended"}; released ${count} process${count === 1 ? "" : "es"}, ${left}.`, at: e.at });
          break;
        }
        case "end":
          call.end = e;
          if (e.usage) {
            s.usage = true;
            s.tokens += e.usage.input + e.usage.output + e.usage.cache_read + e.usage.cache_write;
            s.split.input += e.usage.input;
            s.split.output += e.usage.output;
            s.split.cache_read += e.usage.cache_read;
            s.split.cache_write += e.usage.cache_write;
          }
          break;
      }
    }
    this.seen = end;
    this.first = entries[this.from];
    this.last = entries[end - 1];
    return this;
  }

  private lastPhase: string | undefined;
  private lastPlanLen = -1;
  private lastHalt = "";
  /** An earlier turn of the run: whatever it left open was stopped long ago. */
  private over = false;

  /** Mark this builder as an earlier turn, so a call it left open reads as stopped. */
  closed(over: boolean): this {
    this.over = over;
    return this;
  }

  /** Every agent in plan order, with its cost and state. An unchanged agent is the same object as last time. */
  snapshot(detail: RunDetail | null, ledger: LedgerEntry[]): Surface[] {
    const planPages = detail?.plan?.pages ?? [];
    const pages = planPages.length ? planPages.map((p) => p.id) : (detail?.pages.map((p) => p.id) ?? []);
    // A page the plan split into units is one worker per unit, so six workers stay six blocks.
    const workerIds = pages.flatMap((p) => {
      const planned = planPages.find((page) => page.id === p);
      const prefix = unitPrefix(detail?.plan, planned);
      const units = planned?.units;
      if (units && units.length > 1) return units.map((u) => `${prefix}:${p}/${u.id}`);
      return [`${prefix}:${p}`];
    });
    const order = ["master", ...workerIds, "reviewer", "verifier"];
    for (const id of order) {
      if (!this.byId.has(id)) {
        this.byId.set(id, blankSurface(id));
        this.dirtySummary.add(id);
      }
    }
    const halt = `${detail?.active}:${detail?.cancelled}:${detail?.cancelling ?? ""}`;
    if (this.lastHalt !== halt) {
      this.lastHalt = halt;
      for (const id of this.byId.keys()) this.dirtySummary.add(id);
    }
    if (this.lastPhase !== detail?.phase || this.lastPlanLen !== planPages.length) {
      this.lastPhase = detail?.phase;
      this.lastPlanLen = planPages.length;
      this.dirtySummary.add("master");
    }
    const cost = new Map<string, { usd: number; n: number }>();
    for (const entry of ledger) {
      const key = surfaceOf(entry);
      const c = cost.get(key) ?? { usd: 0, n: 0 };
      c.usd += entry.cost_usd ?? 0;
      c.n += 1;
      cost.set(key, c);
    }
    const halted = this.over || runStopped(detail);
    for (const [id, s] of this.byId) {
      const c = cost.get(id) ?? { usd: 0, n: 0 };
      const open = s.calls.some((call) => !call.end);
      const lastEnd = s.calls.length > 0 ? s.calls[s.calls.length - 1]!.end : undefined;
      const failed = lastEnd?.ok === false && !lastEnd.stopped;
      let state: Surface["state"] =
        s.calls.length === 0 ? "waiting" : open ? (halted ? "stopped" : "working") : lastEnd?.stopped ? "stopped" : failed ? "failed" : "done";
      if (!this.over && s.page && detail?.pages.find((p) => p.id === s.page)?.state === "failed") state = "failed";
      const stateChanged = s.state !== state;
      const dirty = this.dirtySummary.has(id) || stateChanged;
      if (s.cost !== c.usd || s.calls_billed !== c.n || stateChanged || dirty) {
        const next: Surface = { ...s, cost: c.usd, calls_billed: c.n, state };
        if (dirty) next.summary = computeSummary(next, detail);
        this.byId.set(id, next);
      }
    }
    this.dirtySummary.clear();
    const rank = (id: string) => {
      const i = order.indexOf(id);
      return i < 0 ? order.length : i;
    };
    return [...this.byId.values()].sort((a, b) => rank(a.id) - rank(b.id));
  }
}

/** Group transcript entries into calls per agent. */
export function surfaces(detail: RunDetail | null, entries: AgentEntry[], ledger: LedgerEntry[]): Surface[] {
  return new SurfaceBuilder().apply(entries).snapshot(detail, ledger);
}

/** [`surfaces`], kept up to date as entries stream in: only new entries are read, from `from` on. */
export function useSurfaces(detail: RunDetail | null, entries: AgentEntry[], ledger: LedgerEntry[], version = 0, from = 0): Surface[] {
  const builder = useRef<SurfaceBuilder | null>(null);
  return useMemo(() => {
    void version;
    let b = builder.current;
    if (!b || !b.continues(entries, from)) b = builder.current = new SurfaceBuilder().start(from);
    return b.apply(entries).snapshot(detail, ledger);
  }, [detail, entries, ledger, version, from]);
}

/**
 * One turn of a run: the first is the original request; each later one starts where the
 * person's `turn` entry sits in the transcript, after a note on a finished run.
 */
export interface Turn {
  /** 1 for the original request. */
  n: number;
  /** Transcript index the turn starts at, and the index it ends before (open-ended for the last). */
  start: number;
  end: number;
  /** What the person asked, for a later turn. The first turn's is the run's prompt. */
  text: string | null;
  mode: RunMode | null;
  at: string | null;
  /** Ids of the notes this turn took. */
  notes: string[];
  /** The plan in force for the turn, from the run's log. */
  plan: Plan | null;
}

/** Split the transcript into turns at each `turn` entry, with each turn's plan from the log. */
export function useTurns(entries: AgentEntry[], version: number, events: RunEvent[], detail: RunDetail | null): Turn[] {
  const scan = useRef<{ list: AgentEntry[] | null; seen: number; marks: number[] }>({ list: null, seen: 0, marks: [] });
  const marks = useMemo(() => {
    void version;
    const s = scan.current;
    if (s.list !== entries || entries.length < s.seen) {
      s.list = entries;
      s.seen = 0;
      s.marks = [];
    }
    let grew = false;
    for (let i = s.seen; i < entries.length; i++) {
      const e = entries[i]!;
      if (e.kind === "turn" && e.agent.id === "person") {
        s.marks.push(i);
        grew = true;
      }
    }
    s.seen = entries.length;
    if (grew) s.marks = s.marks.slice();
    return s.marks;
  }, [entries, version]);

  const plans = useMemo(() => {
    // The plan before each follow-up, and the one each agent turn brought.
    const out: (Plan | null)[] = [];
    let current: Plan | null = null;
    for (const event of events) {
      if (typeof event !== "object") {
        if (event === "follow_up") out.push(current);
        continue;
      }
      if ("plan_ready" in event) current = event.plan_ready;
      else if ("agent_turn" in event) {
        out.push(current);
        current = event.agent_turn.plan;
      }
    }
    return { before: out, last: current };
  }, [events]);

  return useMemo(() => {
    const total = Number.MAX_SAFE_INTEGER;
    const turns: Turn[] = [];
    const firstEnd = marks.length ? marks[0]! : total;
    turns.push({ n: 1, start: 0, end: firstEnd, text: null, mode: null, at: null, notes: [], plan: plans.before[0] ?? (marks.length ? null : (detail?.plan ?? plans.last)) });
    marks.forEach((at, i) => {
      const e = entries[at]!;
      const end = i + 1 < marks.length ? marks[i + 1]! : total;
      const next = plans.before[i + 1];
      turns.push({
        n: e.turn ?? i + 2,
        start: at,
        end,
        text: e.text ?? null,
        mode: e.mode ?? null,
        at: e.at,
        notes: e.notes ?? [],
        plan: i + 1 < marks.length ? (next ?? null) : (detail?.plan ?? plans.last),
      });
    });
    return turns;
  }, [entries, marks, plans, detail?.plan]);
}

/** The surfaces of a finished turn, worked out once. A call it left open reads as stopped. */
export function turnSurfaces(entries: AgentEntry[], turn: Turn, detail: RunDetail, ledger: LedgerEntry[], until: string | null): Surface[] {
  const past: RunDetail = { ...detail, plan: turn.plan, pages: [], active: false, cancelled: false, cancelling: null, phase: "done" };
  const spent = ledger.filter((e) => (!turn.at || e.at >= turn.at) && (!until || e.at < until));
  return new SurfaceBuilder().start(turn.start).apply(entries, turn.end).closed(true).snapshot(past, spent);
}

export interface RunData {
  detail: RunDetail | null;
  events: RunEvent[];
  entries: AgentEntry[];
  /** Monotonic counter bumped whenever entries are appended or replaced. */
  entriesVersion: number;
  /** P5: true when older calls exist before the bounded recent window. */
  hasOlder: boolean;
  /** The transcript index of the first entry held (P5 loads the recent window first). */
  windowStart: number;
  olderCalls: OlderCallSummary[];
  loadingOlder: boolean;
  loadOlder: () => Promise<void>;
  ledger: { totals: Totals; entries: LedgerEntry[] } | null;
  notes: Note[];
  error: unknown;
  /** The plan arrived while this page was open, so it can be animated in. */
  planArrived: boolean;
  reload: () => Promise<void>;
  refollow: () => void;
  reloadNotes: () => void;
  /** Fetch transcript entries written since the last one, e.g. after a typed command. */
  catchUp: () => Promise<void>;
  /** Show a change at once, before the daemon confirms it; the next load reconciles. */
  patch: (change: Partial<RunDetail>) => void;
}

export function useRun(id: string): RunData {
  // A run prefetched on hover opens with what was fetched, then catches up.
  const [detail, setDetailState] = useState<RunDetail | null>(() => queryClient.getQueryData<RunDetail>(keys.run(id)) ?? null);
  const setDetail = useCallback(
    (d: RunDetail) => {
      queryClient.setQueryData(keys.run(id), d);
      setDetailState(d);
    },
    [id],
  );
  const [events, setEvents] = useState<RunEvent[]>([]);
  // Zero-copy append buffer: streamed batches push in place and bump entriesVersion once per frame.
  const entriesRef = useRef<AgentEntry[]>([]);
  const [entriesVersion, setEntriesVersion] = useState(0);
  const windowStartRef = useRef(0);
  const [hasOlder, setHasOlder] = useState(false);
  const [olderCalls, setOlderCalls] = useState<OlderCallSummary[]>([]);
  const [loadingOlder, setLoadingOlder] = useState(false);
  const [ledger, setLedger] = useState<RunData["ledger"]>(null);
  const [notes, setNotes] = useState<Note[]>([]);
  const [error, setError] = useState<unknown>(null);
  const [planArrived, setPlanArrived] = useState(false);
  const counts = useRef({ events: 0, entries: 0 });
  const stop = useRef<(() => void) | null>(null);
  const loaded = useRef(false);

  const reloadNotes = useCallback(() => {
    api.notes(id).then(setNotes).catch(() => {});
  }, [id]);

  // Incremental ledger fetcher (?after=n): only downloads newly appended ledger rows (P11).
  const ledgerRef = useRef<RunData["ledger"]>(null);
  const fetchLedger = useCallback(async () => {
    const after = ledgerRef.current?.entries.length;
    const res = await api.ledger(id, after);
    const prev = ledgerRef.current;
    let next: NonNullable<RunData["ledger"]>;
    if (prev && after !== undefined && res.after === after && (res.total_entries === undefined || res.total_entries >= after)) {
      next = res.entries.length > 0
        ? { totals: res.totals, entries: [...prev.entries, ...res.entries] }
        : { totals: res.totals, entries: prev.entries };
    } else {
      next = { totals: res.totals, entries: res.entries };
    }
    ledgerRef.current = next;
    setLedger(next);
    return next;
  }, [id]);

  // Coalesce concurrent reloads and debounce SSE bursts within 100ms (P11).
  const reloadInFlight = useRef<Promise<void> | null>(null);
  const reloadQueued = useRef(false);
  const reloadTimer = useRef<number | null>(null);
  const ledgerTimer = useRef<number | null>(null);
  const reload = useCallback(async () => {
    if (reloadTimer.current !== null) {
      window.clearTimeout(reloadTimer.current);
      reloadTimer.current = null;
    }
    if (reloadInFlight.current) {
      reloadQueued.current = true;
      return reloadInFlight.current;
    }
    const runOnce = async () => {
      try {
        const [d] = await Promise.all([api.run(id), fetchLedger()]);
        setDetail(d);
        setError(null);
      } catch (err) {
        setError(err);
      }
      reloadNotes();
    };
    const p = (async () => {
      do {
        reloadQueued.current = false;
        await runOnce();
      } while (reloadQueued.current);
    })().finally(() => {
      reloadInFlight.current = null;
    });
    reloadInFlight.current = p;
    return p;
  }, [id, reloadNotes, setDetail, fetchLedger]);

  const scheduleReload = useCallback(() => {
    if (reloadTimer.current !== null) return;
    reloadTimer.current = window.setTimeout(() => {
      reloadTimer.current = null;
      void reload();
    }, 100);
  }, [reload]);

  const scheduleLedger = useCallback(() => {
    if (ledgerTimer.current !== null) return;
    ledgerTimer.current = window.setTimeout(() => {
      ledgerTimer.current = null;
      void fetchLedger().catch(() => undefined);
    }, 100);
  }, [fetchLedger]);

  useEffect(
    () => () => {
      if (reloadTimer.current !== null) window.clearTimeout(reloadTimer.current);
      if (ledgerTimer.current !== null) window.clearTimeout(ledgerTimer.current);
      reloadTimer.current = null;
      ledgerTimer.current = null;
      ledgerRef.current = null;
    },
    [id],
  );

  // Flush streamed entries on the very next display frame (zero artificial throttle) so
  // high-refresh displays render smoothly with at most one React commit per frame.
  const pending = useRef<AgentEntry[]>([]);
  const frame = useRef(0);
  const push = useCallback((entry: AgentEntry) => {
    pending.current.push(entry);
    if (frame.current) return;
    frame.current = requestAnimationFrame(() => {
      frame.current = 0;
      const batch = pending.current;
      pending.current = [];
      if (batch.length) {
        entriesRef.current.push(...batch);
        setEntriesVersion((v) => v + 1);
      }
    });
  }, []);
  useEffect(
    () => () => {
      if (frame.current) cancelAnimationFrame(frame.current);
    },
    [],
  );

  const catchUp = useCallback(async () => {
    // While the stream is open it delivers the entries itself.
    if (stop.current) return;
    try {
      const ag = await api.agents(id, counts.current.entries, { stub: true });
      if (!ag.entries.length || ag.next <= counts.current.entries) return;
      counts.current.entries = ag.next;
      entriesRef.current.push(...ag.entries);
      setEntriesVersion((v) => v + 1);
    } catch {
      // The poll below picks it up.
    }
  }, [id]);

  const loadOlder = useCallback(async () => {
    const before = windowStartRef.current;
    if (before <= 0) return;
    setLoadingOlder(true);
    try {
      const ag = await api.agents(id, 0, { before });
      const nextStart = ag.window_start ?? 0;
      windowStartRef.current = nextStart;
      setHasOlder(!!ag.has_older && nextStart > 0);
      setOlderCalls(ag.older_calls ?? []);
      if (ag.entries.length) {
        entriesRef.current = [...ag.entries, ...entriesRef.current];
        setEntriesVersion((v) => v + 1);
      }
    } finally {
      setLoadingOlder(false);
    }
  }, [id]);

  const refollow = useCallback(() => {
    stop.current?.();
    stop.current = follow(
      id,
      counts.current.events,
      {
        event: (event, index) => {
          if (index <= counts.current.events) return;
          counts.current.events = index;
          setEvents((all) => [...all, event]);
          if (typeof event === "object" && "plan_ready" in event && loaded.current) setPlanArrived(true);
          if (typeof event === "object" && "worker_finished" in event && loaded.current) {
            const { page, outcome } = event.worker_finished;
            if (outcome === "passed") pageDone(id, page);
            else if (typeof outcome === "object" && "check_failed" in outcome) checksFailed(id, page, outcome.check_failed.errors);
          }
          scheduleReload();
        },
        agent: (entry) => {
          counts.current.entries += 1;
          push(entry);
          // A finished call is in the ledger now: refresh only new ledger rows, coalesced.
          if (entry.kind === "end") scheduleLedger();
        },
        idle: () => {
          stop.current = null;
          void reload();
        },
        lost: () => {
          stop.current = null;
        },
      },
      counts.current.entries,
    );
  }, [id, reload, scheduleReload, scheduleLedger, push]);

  useEffect(() => {
    let stale = false;
    counts.current = { events: 0, entries: 0 };
    windowStartRef.current = 0;
    setHasOlder(false);
    setOlderCalls([]);
    loaded.current = false;
    pending.current = [];
    if (frame.current) {
      cancelAnimationFrame(frame.current);
      frame.current = 0;
    }
    setEvents([]);
    entriesRef.current = [];
    setEntriesVersion((v) => v + 1);
    setPlanArrived(false);
    void (async () => {
      try {
        const [ev, ag] = await Promise.all([api.events(id), api.agents(id, 0, { window: "recent" })]);
        if (stale) return;
        counts.current = { events: ev.next, entries: ag.next };
        windowStartRef.current = ag.window_start ?? 0;
        setHasOlder(!!ag.has_older && (ag.window_start ?? 0) > 0);
        setOlderCalls(ag.older_calls ?? []);
        setEvents(ev.events);
        entriesRef.current = ag.entries;
        setEntriesVersion((v) => v + 1);
        await reload();
        loaded.current = true;
        refollow();
      } catch (err) {
        if (!stale) setError(err);
      }
    })();
    return () => {
      stale = true;
      stop.current?.();
      stop.current = null;
    };
  }, [id, reload, refollow]);

  // A run that was started again elsewhere (or by a button here) gets followed again.
  useEffect(() => {
    const timer = window.setInterval(async () => {
      if (document.visibilityState !== "visible" || stop.current || !loaded.current) return;
      try {
        const d = await api.run(id);
        setDetail(d);
        const [ev, ag] = await Promise.all([
          api.events(id, counts.current.events),
          api.agents(id, counts.current.entries, { stub: true }),
        ]);
        if (ev.events.length || ag.entries.length || d.active) {
          if (ag.entries.length) {
            counts.current.entries = ag.next;
            entriesRef.current.push(...ag.entries);
            setEntriesVersion((v) => v + 1);
          }
          if (ev.events.length) {
            counts.current.events = ev.next;
            setEvents((all) => [...all, ...ev.events]);
            void reload();
          }
          if (d.active) refollow();
        }
      } catch {
        // The next tick tries again.
      }
    }, 2500);
    return () => window.clearInterval(timer);
  }, [id, refollow, reload, setDetail]);

  const patch = useCallback((change: Partial<RunDetail>) => setDetailState((d) => (d ? { ...d, ...change } : d)), []);

  return {
    detail,
    events,
    entries: entriesRef.current,
    entriesVersion,
    hasOlder,
    /** The transcript index of the first entry held (P5 loads the recent window first). */
    windowStart: windowStartRef.current,
    olderCalls,
    loadingOlder,
    loadOlder,
    ledger,
    notes,
    error,
    planArrived,
    reload,
    refollow,
    reloadNotes,
    catchUp,
    patch,
  };
}

/**
 * True once nothing is driving the run any more, or a force stop was asked for: an agent
 * call with no end then reads as stopped instead of spinning forever.
 */
export function runStopped(detail: RunDetail | null | undefined): boolean {
  if (!detail) return false;
  if (detail.cancelling === "force") return true;
  if (detail.active) return false;
  return detail.cancelled || detail.phase === "done" || detail.phase === "failed" || detail.phase === "awaiting_approval";
}

/** A page's state as shown: a page still marked building on a run that stopped is stopped. */
export function shownPageState(state: PageState | undefined, detail: RunDetail | null | undefined): PageState | "stopped" | undefined {
  if (state === "building" && runStopped(detail)) return "stopped";
  return state;
}

/**
 * The paths and patterns a glob names, one per entry: `{a.html,b.css,tests/**}` is three,
 * `src/{a,b}.ts` is two. Nested braces expand too.
 */
export function expandGlob(glob: string): string[] {
  const text = glob.trim();
  if (!text) return [];
  const open = text.indexOf("{");
  if (open < 0) return text.split(/\s*,\s*/).filter(Boolean);
  let depth = 0;
  let close = -1;
  for (let i = open; i < text.length; i++) {
    if (text[i] === "{") depth++;
    else if (text[i] === "}" && --depth === 0) {
      close = i;
      break;
    }
  }
  if (close < 0) return [text];
  const head = text.slice(0, open);
  const tail = text.slice(close + 1);
  const parts: string[] = [];
  let start = open + 1;
  depth = 0;
  for (let i = open + 1; i < close; i++) {
    if (text[i] === "{") depth++;
    else if (text[i] === "}") depth--;
    else if (text[i] === "," && depth === 0) {
      parts.push(text.slice(start, i));
      start = i + 1;
    }
  }
  parts.push(text.slice(start, close));
  const out: string[] = [];
  for (const part of parts) {
    for (const each of expandGlob(`${head}${part.trim()}${tail}`)) if (!out.includes(each)) out.push(each);
  }
  return out;
}

/** A glob as the daemon's owners read it: `**` any depth, `*` within a segment, `{a,b}` either. */
export function globMatch(glob: string, path: string): boolean {
  return expandGlob(glob).some((one) => {
    const re = one
      .split("**")
      .map((part) => part.split("*").map((s) => s.replace(/[.+?^${}()|[\]\\]/g, "\\$&")).join("[^/]*"))
      .join(".*");
    return new RegExp(`^${re.replace(/\.\*\//g, "(?:.*/)?")}$`).test(path);
  });
}
