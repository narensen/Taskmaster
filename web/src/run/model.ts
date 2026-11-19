// A run as the control surface sees it: its log, its agents' transcript, its notes and its
// ledger, followed live over the daemon's stream.

import { useCallback, useEffect, useRef, useState } from "react";
import {
  api,
  follow,
  type AgentEntry,
  type AgentTag,
  type LedgerEntry,
  type Note,
  type RunDetail,
  type RunEvent,
  type Totals,
} from "../api";
import { keys, queryClient } from "../query";
import { checksFailed, pageDone } from "../notify";

export interface ToolItem {
  t: "tool";
  name: string;
  target: string | null;
  allowed?: boolean;
  input?: unknown;
  /** Text the harness printed for this call, when it sent any. */
  output?: string;
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

export interface Surface {
  id: string;
  title: string;
  role: string;
  page: string | null;
  calls: Call[];
  tag: AgentTag | null;
  state: "waiting" | "working" | "done" | "failed";
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

export function surfaceOf(entry: LedgerEntry): string {
  if (["planner", "master", "arbiter"].includes(entry.role)) return "master";
  if ((entry.role === "worker" || entry.role === "mid") && entry.page) return `page:${entry.page}`;
  if (entry.role === "security" || entry.role === "reviewer") return "reviewer";
  return entry.role;
}

/** Group transcript entries into calls per agent. */
export function surfaces(detail: RunDetail | null, entries: AgentEntry[], ledger: LedgerEntry[]): Surface[] {
  const planPages = detail?.plan?.pages ?? [];
  const pages = planPages.map((p) => p.id).length ? planPages.map((p) => p.id) : (detail?.pages.map((p) => p.id) ?? []);
  // A page the plan split into units is one worker per unit, so six workers stay six blocks.
  const workerIds = pages.flatMap((p) => {
    const units = planPages.find((page) => page.id === p)?.units;
    if (units && units.length > 1) return units.map((u) => `page:${p}/${u.id}`);
    return [`page:${p}`];
  });
  const order = ["master", ...workerIds, "reviewer", "verifier"];
  const byId = new Map<string, Surface>();
  const make = (id: string): Surface => {
    const raw = id.startsWith("page:") ? id.slice(5) : null;
    const page = raw ? raw.split("/")[0]! : null;
    const role = page ? "worker" : id;
    return {
      id,
      title: raw ? `Worker · ${raw.replaceAll("/", " / ")}` : id[0]!.toUpperCase() + id.slice(1),
      role,
      page,
      calls: [],
      tag: null,
      state: "waiting",
      tokens: 0,
      split: { input: 0, output: 0, cache_read: 0, cache_write: 0 },
      usage: false,
      cost: 0,
      calls_billed: 0,
    };
  };
  for (const id of order) byId.set(id, make(id));
  const calls = new Map<string, Call>();
  for (const e of entries) {
    const id = e.agent.id;
    if (id === "runtime" || id === "person") continue;
    if (!byId.has(id)) byId.set(id, make(id));
    const s = byId.get(id)!;
    const key = `${e.agent.id}#${e.agent.call ?? 0}`;
    let call = calls.get(key);
    if (!call) {
      call = { call: e.agent.call ?? 0, tag: e.agent, items: [], at: e.at };
      calls.set(key, call);
      s.calls.push(call);
    }
    s.tag = e.agent;
    const last = call.items[call.items.length - 1];
    switch (e.kind) {
      case "start":
        call.brief = e.brief;
        break;
      case "text":
        if (last?.t === "text") last.text += e.text ?? "";
        else call.items.push({ t: "text", text: e.text ?? "", at: e.at });
        break;
      case "thought":
        if (last?.t === "thought") last.text += e.text ?? "";
        else call.items.push({ t: "thought", text: e.text ?? "", at: e.at });
        break;
      case "tool_call":
        call.items.push({ t: "tool", name: e.name ?? "tool", target: e.target ?? null, input: e.input, at: e.at });
        break;
      case "command":
        call.items.push({ t: "tool", name: "command", target: e.command ?? null, at: e.at });
        break;
      case "output": {
        const open = [...call.items].reverse().find((i): i is ToolItem => i.t === "tool" && i.output === undefined);
        if (open) open.output = e.text ?? "";
        else if (e.text) call.items.push({ t: "text", text: e.text, at: e.at });
        break;
      }
      case "preview":
        call.items.push({ t: "tool", name: "browser", target: e.routes?.map((r) => r.route).join(" ") || e.step || null, at: e.at });
        break;
      case "permission": {
        const open = [...call.items].reverse().find((i): i is ToolItem => i.t === "tool" && i.allowed === undefined && i.name === (e.tool ?? e.name));
        if (open) open.allowed = e.allowed;
        else call.items.push({ t: "tool", name: e.tool ?? "tool", target: null, allowed: e.allowed, at: e.at });
        break;
      }
      case "stalled":
        call.items.push({ t: "life", text: `Stalled waiting on its model; fresh agent ${e.restart ?? 1} started.`, at: e.at });
        break;
      case "released": {
        const left = e.lingering ? `${e.lingering} still running` : "nothing left running";
        const n = e.processes ?? 0;
        call.items.push({ t: "life", text: `${e.asked ? "Reported done and asked to be released" : "Turn ended"}; released ${n} process${n === 1 ? "" : "es"}, ${left}.`, at: e.at });
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
  for (const entry of ledger) {
    const s = byId.get(surfaceOf(entry));
    if (!s) continue;
    s.cost += entry.cost_usd ?? 0;
    s.calls_billed += 1;
  }
  for (const s of byId.values()) {
    const open = s.calls.some((c) => !c.end);
    const failed = s.calls.length > 0 && s.calls[s.calls.length - 1]!.end?.ok === false;
    s.state = s.calls.length === 0 ? "waiting" : open ? "working" : failed ? "failed" : "done";
    if (s.page) {
      const page = detail?.pages.find((p) => p.id === s.page);
      if (page?.state === "failed") s.state = "failed";
    }
  }
  const rank = (id: string) => {
    const i = order.indexOf(id);
    return i < 0 ? order.length : i;
  };
  return [...byId.values()].sort((a, b) => rank(a.id) - rank(b.id));
}

export interface RunData {
  detail: RunDetail | null;
  events: RunEvent[];
  entries: AgentEntry[];
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
  const [entries, setEntries] = useState<AgentEntry[]>([]);
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

  const reload = useCallback(async () => {
    try {
      const [d, l] = await Promise.all([api.run(id), api.ledger(id)]);
      setDetail(d);
      setLedger(l);
      setError(null);
    } catch (err) {
      setError(err);
    }
    reloadNotes();
  }, [id, reloadNotes, setDetail]);

  // Streamed entries are added once a frame, however fast they arrive.
  const pending = useRef<AgentEntry[]>([]);
  const frame = useRef(0);
  const push = useCallback((entry: AgentEntry) => {
    pending.current.push(entry);
    if (frame.current) return;
    frame.current = requestAnimationFrame(() => {
      frame.current = 0;
      const batch = pending.current;
      pending.current = [];
      setEntries((all) => all.concat(batch));
    });
  }, []);
  useEffect(() => () => cancelAnimationFrame(frame.current), []);

  const catchUp = useCallback(async () => {
    // While the stream is open it delivers the entries itself.
    if (stop.current) return;
    try {
      const ag = await api.agents(id, counts.current.entries);
      if (!ag.entries.length || ag.next <= counts.current.entries) return;
      counts.current.entries = ag.next;
      setEntries((all) => [...all, ...ag.entries]);
    } catch {
      // The poll below picks it up.
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
          void reload();
        },
        agent: (entry) => {
          counts.current.entries += 1;
          push(entry);
          // A finished call is in the ledger now: refresh the token split.
          if (entry.kind === "end") api.ledger(id).then(setLedger, () => undefined);
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
  }, [id, reload, push]);

  useEffect(() => {
    let stale = false;
    counts.current = { events: 0, entries: 0 };
    loaded.current = false;
    setEvents([]);
    setEntries([]);
    setPlanArrived(false);
    void (async () => {
      try {
        const [ev, ag] = await Promise.all([api.events(id), api.agents(id)]);
        if (stale) return;
        counts.current = { events: ev.next, entries: ag.next };
        setEvents(ev.events);
        setEntries(ag.entries);
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
        const [ev, ag] = await Promise.all([api.events(id, counts.current.events), api.agents(id, counts.current.entries)]);
        if (ev.events.length || ag.entries.length || d.active) {
          if (ag.entries.length) {
            counts.current.entries = ag.next;
            setEntries((all) => [...all, ...ag.entries]);
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

  return { detail, events, entries, ledger, notes, error, planArrived, reload, refollow, reloadNotes, catchUp, patch };
}

/** A glob as the daemon's owners read it: `**` any depth, `*` within a segment. */
export function globMatch(glob: string, path: string): boolean {
  const re = glob
    .split("**")
    .map((part) => part.split("*").map((s) => s.replace(/[.+?^${}()|[\]\\]/g, "\\$&")).join("[^/]*"))
    .join(".*");
  return new RegExp(`^${re}$`).test(path);
}
