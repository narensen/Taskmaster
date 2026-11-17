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

export interface ToolItem {
  t: "tool";
  name: string;
  target: string | null;
  allowed?: boolean;
  input?: unknown;
}
export type Item = { t: "text"; text: string } | { t: "thought"; text: string } | ToolItem;

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
  usage: boolean;
  cost: number;
  calls_billed: number;
}

export function surfaceOf(entry: LedgerEntry): string {
  if (["planner", "master", "arbiter"].includes(entry.role)) return "master";
  if ((entry.role === "worker" || entry.role === "mid") && entry.page) return `page:${entry.page}`;
  if (entry.role === "security" || entry.role === "reviewer") return "reviewer";
  return entry.role;
}

/** Group transcript entries into calls per agent. */
export function surfaces(detail: RunDetail | null, entries: AgentEntry[], ledger: LedgerEntry[]): Surface[] {
  const pages = detail?.plan?.pages.map((p) => p.id) ?? detail?.pages.map((p) => p.id) ?? [];
  const order = ["master", ...pages.map((p) => `page:${p}`), "reviewer", "verifier"];
  const byId = new Map<string, Surface>();
  const make = (id: string): Surface => {
    const page = id.startsWith("page:") ? id.slice(5) : null;
    const role = page ? "worker" : id;
    return {
      id,
      title: page ? `Worker · ${page}` : id[0]!.toUpperCase() + id.slice(1),
      role,
      page,
      calls: [],
      tag: null,
      state: "waiting",
      tokens: 0,
      usage: false,
      cost: 0,
      calls_billed: 0,
    };
  };
  for (const id of order) byId.set(id, make(id));
  const calls = new Map<string, Call>();
  for (const e of entries) {
    // A split page's units (`page:<page>/<unit>`) are separate agents shown on the page's surface.
    const id = e.agent.id.startsWith("page:") ? e.agent.id.split("/")[0]! : e.agent.id;
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
        else call.items.push({ t: "text", text: e.text ?? "" });
        break;
      case "thought":
        if (last?.t === "thought") last.text += e.text ?? "";
        else call.items.push({ t: "thought", text: e.text ?? "" });
        break;
      case "tool_call":
        call.items.push({ t: "tool", name: e.name ?? "tool", target: e.target ?? null, input: e.input });
        break;
      case "permission": {
        const open = [...call.items].reverse().find((i): i is ToolItem => i.t === "tool" && i.allowed === undefined && i.name === e.tool);
        if (open) open.allowed = e.allowed;
        else call.items.push({ t: "tool", name: e.tool ?? "tool", target: null, allowed: e.allowed });
        break;
      }
      case "end":
        call.end = e;
        if (e.usage) {
          s.usage = true;
          s.tokens += e.usage.input + e.usage.output + e.usage.cache_read + e.usage.cache_write;
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
}

export function useRun(id: string): RunData {
  const [detail, setDetail] = useState<RunDetail | null>(null);
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
  }, [id, reloadNotes]);

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
          void reload();
        },
        agent: (entry) => {
          counts.current.entries += 1;
          setEntries((all) => [...all, entry]);
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
  }, [id, reload]);

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
  }, [id, refollow, reload]);

  return { detail, events, entries, ledger, notes, error, planArrived, reload, refollow, reloadNotes, catchUp };
}

/** A glob as the daemon's owners read it: `**` any depth, `*` within a segment. */
export function globMatch(glob: string, path: string): boolean {
  const re = glob
    .split("**")
    .map((part) => part.split("*").map((s) => s.replace(/[.+?^${}()|[\]\\]/g, "\\$&")).join("[^/]*"))
    .join(".*");
  return new RegExp(`^${re}$`).test(path);
}
