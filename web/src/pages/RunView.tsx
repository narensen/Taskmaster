// A run: the master, each page worker, the reviewer and the verifier as their own
// surfaces, the live plan, approvals, the notes waiting for the next brief, and a right
// panel with the plan, the diff, the browser, web search, the terminal and file previews.

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { motion } from "framer-motion";
import gsap from "gsap";
import { api, type RunDetail } from "../api";
import { setRunContext } from "../commands";
import { Composer, messageText, type ComposerHandle } from "../composer";
import { phaseLabel, usd } from "../format";
import { emit, Sheet, toast, useOn, type Chip } from "../overlay";
import { ExternalLink, Rich, label, resolvedPath } from "../paths";
import { navigate } from "../router";
import { motionOn, prefs, setPrefs } from "../prefs";
import { bezier, dur, ease } from "../motion";
import { Problem, Spinner, useLoad } from "../ui";
import { takeAttachments } from "../App";
import { AgentSurface } from "../run/AgentSurface";
import { DiffTab, type Scope } from "../run/Diff";
import { PlanView } from "../run/PlanView";
import { BrowserTab, PreviewTab, SearchTab, TerminalTab } from "../run/Tabs";
import { surfaces, useRun } from "../run/model";

const FIXED = ["plan", "diff", "browser", "search", "terminal"] as const;
const TAB_LABEL: Record<string, string> = { plan: "Plan", diff: "Diff", browser: "Browser", search: "Web search", terminal: "Terminal" };

function useNarrow(): boolean {
  const [narrow, setNarrow] = useState(() => window.matchMedia("(max-width: 900px)").matches);
  useEffect(() => {
    const q = window.matchMedia("(max-width: 900px)");
    const on = () => setNarrow(q.matches);
    q.addEventListener("change", on);
    return () => q.removeEventListener("change", on);
  }, []);
  return narrow;
}

function ApprovalCard({ id, detail, composer, onDone }: { id: string; detail: RunDetail; composer: React.RefObject<ComposerHandle | null>; onDone: () => void }) {
  const [reply, setReply] = useState("");
  const [busy, setBusy] = useState(false);
  const plan = detail.plan;
  const approve = async () => {
    setBusy(true);
    try {
      // A draft in the composer rides along as the note on this approval.
      const draft = composer.current && !composer.current.isEmpty() ? messageText(composer.current.read()) : undefined;
      await api.approve(id, draft);
      composer.current?.clear();
      onDone();
    } catch (err) {
      toast(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };
  const reject = async () => {
    setBusy(true);
    try {
      // A text reply rejects this plan; the master plans again with it as the note.
      const prompt = reply.trim() ? `${detail.prompt ?? ""}\n\nThe person rejected the last plan: ${reply.trim()}`.trim() : undefined;
      await api.reject(id, prompt);
      setReply("");
      onDone();
    } catch (err) {
      toast(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };
  return (
    <motion.div
      className="approval"
      data-testid="approval"
      initial={motionOn() ? { opacity: 0, y: 8 } : false}
      animate={{ opacity: 1, y: 0 }}
      transition={{ duration: 0.18, ease: bezier.out }}
    >
      <div className="strong">The master asks to build this plan</div>
      <div className="small muted">
        {plan ? `${plan.pages.length} page(s): ${plan.pages.map((p) => `${p.id} → ${p.owned_glob}`).join(", ")}` : "a plan"} · Build stays blocked until you approve.
      </div>
      <textarea
        rows={2}
        value={reply}
        placeholder="Reply in words to reject it and plan again with your note"
        onChange={(e) => setReply(e.target.value)}
      />
      <div className="row gap wrap">
        <button className="primary" disabled={busy} onClick={() => void approve()}>
          Approve
        </button>
        <button disabled={busy} onClick={() => void reject()}>
          {reply.trim() ? "Reject with reply" : "Reject"}
        </button>
      </div>
    </motion.div>
  );
}

export function RunView({ id }: { id: string }) {
  const data = useRun(id);
  const { detail, entries, ledger, notes } = data;
  const narrow = useNarrow();
  const composer = useRef<ComposerHandle>(null);
  const settings = useLoad(api.settings, []);
  const gh = useLoad(api.gh, []);
  const [focus, setFocus] = useState<string | null>(null);
  const [to, setTo] = useState("master");
  const [sheet, setSheet] = useState(false);
  const [cite, setCite] = useState<{ x: number; y: number; text: string; agent: string } | null>(null);
  const [citeNote, setCiteNote] = useState("");
  const [pr, setPr] = useState<string | null>(null);
  const panelBody = useRef<HTMLDivElement>(null);
  const citeRef = useRef<HTMLDivElement>(null);

  // The right panel: fixed tabs plus file previews; the active one is kept per run.
  const [tabs, setTabs] = useState<string[]>([...FIXED]);
  const [closed, setClosed] = useState<string[]>([]);
  const [tab, setTabState] = useState<string>(() => prefs().panelTab[id] ?? "plan");
  const [previews, setPreviews] = useState<Record<string, { path: string; line?: number }>>({});
  const [diffRequest, setDiffRequest] = useState<{ scope: Scope; page?: string; attempt?: number; n: number } | null>(null);
  const setTab = useCallback(
    (t: string) => {
      setTabState(t);
      setTabs((all) => (all.includes(t) ? all : [...all, t]));
      setPrefs((p) => ({ panelTab: { ...p.panelTab, [id]: t } }));
    },
    [id],
  );

  const list = useMemo(() => surfaces(detail, entries, ledger?.entries ?? []), [detail, entries, ledger]);
  const master = list.find((s) => s.id === "master");
  const workers = list.filter((s) => s.page);
  const rest = list.filter((s) => s.id !== "master" && !s.page);
  const maxTokens = Math.max(1, ...list.map((s) => s.tokens));
  const pages = detail?.plan?.pages.map((p) => p.id) ?? detail?.pages.map((p) => p.id) ?? [];
  const phase = detail?.phase;

  const focusAgent = useCallback((agent: string) => {
    setFocus(agent);
    const el = document.getElementById(`agent-${agent}`);
    el?.scrollIntoView({ block: "start", behavior: "smooth" });
    el?.focus({ preventScroll: true });
    setTo(agent.startsWith("page:") ? agent : agent === "master" || agent === "reviewer" || agent === "verifier" ? agent : "master");
  }, []);

  const openTab = useCallback(
    (t: string) => {
      setTab(t);
      if (narrow) setSheet(true);
    },
    [narrow, setTab],
  );

  const approveNow = useCallback(async () => {
    await api.approve(id).catch((err) => toast(err.message));
    await data.reload();
    data.refollow();
  }, [id, data]);

  useEffect(() => {
    setRunContext({
      id,
      pages,
      focusAgent,
      openTab,
      approve: phase === "awaiting_approval" ? () => void approveNow() : undefined,
      reject: phase === "awaiting_approval" ? () => void api.reject(id).then(() => data.reload()) : undefined,
    });
    return () => setRunContext(null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id, pages.join(","), phase, focusAgent, openTab, approveNow]);

  // Files dropped on this run's card elsewhere.
  useEffect(() => {
    const waiting = takeAttachments(id);
    if (waiting.length) window.setTimeout(() => composer.current?.attach(waiting), 50);
  }, [id]);

  useOn<{ path: string; line?: number }>("preview", ({ path, line }) => {
    const key = `file:${path}`;
    setPreviews((p) => ({ ...p, [key]: { path, line } }));
    setTab(key);
    if (narrow) setSheet(true);
  });
  useOn<{ scope: Scope; page?: string; attempt?: number }>("diff", (req) => {
    setDiffRequest({ ...req, n: Date.now() });
    openTab("diff");
  });
  useOn<Chip>("chip-jump", (chip) => {
    if (chip.kind === "file") {
      const full = resolvedPath(chip.value) ?? chip.value;
      emit("preview", { path: full, line: chip.line });
    } else if (chip.kind === "page") focusAgent(`page:${chip.value}`);
    else if (chip.kind === "agent") focusAgent(chip.value.startsWith("worker:") ? `page:${chip.value.slice(7)}` : chip.value);
    else if (chip.kind === "key") navigate(`/dictionary?q=${encodeURIComponent(chip.value)}`);
  });
  useOn<Chip>("chip", (chip) => {
    if (chip.to) setTo(chip.to.startsWith("page:") || ["master", "reviewer", "verifier", "workers"].includes(chip.to) ? chip.to : "master");
  });

  // Command-W closes the focused tab; Command-Shift-T opens the last one closed.
  const closeTab = useCallback(
    (t: string) => {
      setTabs((all) => {
        const next = all.filter((x) => x !== t);
        if (t === tab) setTabState(next[next.length - 1] ?? "plan");
        return next;
      });
      setClosed((c) => [...c, t]);
    },
    [tab],
  );
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const mod = e.metaKey || e.ctrlKey;
      if (mod && !e.shiftKey && e.key.toLowerCase() === "w") {
        e.preventDefault();
        closeTab(tab);
      } else if (mod && e.shiftKey && e.key.toLowerCase() === "t") {
        e.preventDefault();
        const last = closed[closed.length - 1];
        if (last) {
          setClosed((c) => c.slice(0, -1));
          setTab(last);
        }
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [tab, closed, closeTab, setTab]);

  useLayoutEffect(() => {
    if (!panelBody.current || !motionOn()) return;
    gsap.fromTo(panelBody.current, { opacity: 0, y: 4 }, { opacity: 1, y: 0, duration: dur.fast, ease: ease.out, clearProps: "opacity,transform" });
  }, [tab]);

  useLayoutEffect(() => {
    if (!cite || !citeRef.current || !motionOn()) return;
    gsap.fromTo(citeRef.current, { opacity: 0, y: 4 }, { opacity: 1, y: 0, duration: dur.fast, ease: ease.out, clearProps: "opacity,transform" });
  }, [cite]);

  // Cite: select text in an agent's reply.
  useEffect(() => {
    const onUp = () => {
      const sel = window.getSelection();
      const text = sel?.toString().trim();
      if (!sel || !text || !sel.rangeCount) return;
      const node = sel.anchorNode instanceof Element ? sel.anchorNode : sel.anchorNode?.parentElement;
      const box = node?.closest("[data-agent-text]") as HTMLElement | null;
      if (!box) return;
      const rect = sel.getRangeAt(0).getBoundingClientRect();
      setCite({ x: rect.left + rect.width / 2, y: rect.top, text, agent: box.dataset.agentText! });
      setCiteNote("");
    };
    document.addEventListener("mouseup", onUp);
    return () => document.removeEventListener("mouseup", onUp);
  }, []);

  const send = async (message: { text: string; chips: Chip[]; attachments: Chip[] }) => {
    const text = messageText(message);
    if (!text) return;
    try {
      await api.addNote(id, to, text);
      composer.current?.clear();
      data.reloadNotes();
    } catch (err) {
      toast(err instanceof Error ? err.message : String(err));
    }
  };

  if (data.error && !detail) return <Problem error={data.error} onRetry={() => void data.reload()} />;
  if (!detail) return <Spinner label="Opening the run" />;

  const recipients = ["master", "workers", ...pages.map((p) => `page:${p}`), "reviewer", "verifier"];
  const pending = notes.filter((n) => !n.delivered);
  const delivered = notes.filter((n) => n.delivered);
  const panelTabs = tabs;
  const panel = (
    <div className="panel" data-testid="right-panel">
      <div className="panel-tabs" role="tablist">
        {panelTabs.map((t) => (
          <div key={t} className={`panel-tab ${t === tab ? "on" : ""}`} role="tab" aria-selected={t === tab}>
            <button className="panel-tab-name" onClick={() => setTab(t)}>
              {t.startsWith("file:") ? label(previews[t]?.path ?? t.slice(5)) : TAB_LABEL[t]}
            </button>
            <button className="chip-x" aria-label={`Close ${t}`} onClick={() => closeTab(t)}>
              ×
            </button>
          </div>
        ))}
        {closed.length > 0 && (
          <button className="ghost small" title="Reopen the last closed tab (⌘⇧T)" onClick={() => setTab(closed[closed.length - 1]!)}>
            +
          </button>
        )}
      </div>
      <div className="panel-body" ref={panelBody}>
        {tab === "plan" && (detail.plan ? <PlanView plan={detail.plan} /> : <div className="pad muted">No plan yet. The master is still planning.</div>)}
        {tab === "diff" && <DiffTab run={id} detail={detail} request={diffRequest} />}
        {tab === "browser" && <BrowserTab run={id} detail={detail} entries={entries} />}
        {tab === "search" && <SearchTab />}
        {tab === "terminal" && <TerminalTab run={id} entries={entries} project={settings.data?.project ?? ""} onRan={() => void data.catchUp()} />}
        {tab.startsWith("file:") && previews[tab] && <PreviewTab path={previews[tab]!.path} line={previews[tab]!.line} />}
        {!panelTabs.includes(tab) && <div className="pad muted">Every tab is closed. ⌘⇧T reopens the last one.</div>}
      </div>
    </div>
  );

  return (
    <div className={`run ${narrow ? "narrow" : ""}`}>
      <div className="run-main">
        <div className="run-scroll">
        <header className="run-head">
          <div className="grow">
            <div className="eyebrow">{settings.data?.project ?? "run"}</div>
            <h1 className="break">{id}</h1>
            <div className="run-meta small">
              <span className={`phase phase-${phase}`}>{phaseLabel(detail)}</span>
              {detail.active && <span> · live</span>}
              {detail.branch && <span> · {detail.branch}</span>}
              <span> · {usd(ledger?.totals.cost_usd ?? detail.spent.cost_usd)} spent</span>
              {detail.estimate && <span> · estimate {usd(detail.estimate.usd)}</span>}
            </div>
            {detail.prompt && (
              <p className="prompt small">
                <Rich text={detail.prompt.split("\n")[0] ?? ""} />
              </p>
            )}
            {detail.error && (
              <div className="error-line small">
                <Rich text={detail.error} />
              </div>
            )}
          </div>
          <div className="row gap wrap">
            {!detail.active && !["done", "failed", "awaiting_approval"].includes(detail.phase) && (
              <button onClick={() => void api.resume(id).then(() => (data.refollow(), data.reload()))}>Resume</button>
            )}
            {(detail.active || detail.phase === "awaiting_approval") && (
              <button className="ghost" onClick={() => void api.cancel(id).then(() => data.reload())}>
                Cancel
              </button>
            )}
            {detail.phase === "done" && gh.data?.account && detail.branch && (
              <button
                className="primary"
                onClick={() =>
                  void api
                    .draftPr(id)
                    .then((r) => setPr(r.url))
                    .catch((err) => toast("The pull request was not drafted", err.message))
                }
              >
                Draft pull request
              </button>
            )}
            {pr && <ExternalLink href={pr}>pull request</ExternalLink>}
            {narrow && (
              <button className="ghost" onClick={() => setSheet(true)}>
                Panel
              </button>
            )}
          </div>
        </header>

        {phase === "planning" && (
          <div className="banner small">
            Planning. The master's text appears below as it arrives; the locked plan replaces this line when it is ready.
          </div>
        )}
        {detail.plan && <PlanView plan={detail.plan} arrived={data.planArrived} compact />}

        <div className="agents">
          {master && (
            <AgentSurface surface={master} detail={detail} maxTokens={maxTokens} events={data.events.length} focused={focus === "master"}>
              {phase === "awaiting_approval" && (
                <ApprovalCard
                  id={id}
                  detail={detail}
                  composer={composer}
                  onDone={() => {
                    void data.reload();
                    data.refollow();
                  }}
                />
              )}
            </AgentSurface>
          )}
          {workers.length > 0 && (
            <div className="workers">
              {workers.map((s) => (
                <AgentSurface key={s.id} surface={s} detail={detail} maxTokens={maxTokens} events={data.events.length} focused={focus === s.id} />
              ))}
            </div>
          )}
          <div className="workers">
            {rest.map((s) => (
              <AgentSurface key={s.id} surface={s} detail={detail} maxTokens={maxTokens} events={data.events.length} focused={focus === s.id} />
            ))}
          </div>
        </div>

        <section className="drafts">
          {pending.map((n) => (
            <div key={n.id} className="draft" data-testid="draft">
              <div className="row between small">
                <span className="muted">waiting for the next brief to {n.to}</span>
                <span className="row gap">
                  <button
                    className="link small"
                    onClick={() => {
                      const text = window.prompt("Edit the note", n.text);
                      if (text !== null) void api.editNote(id, n.id, text || undefined).then(data.reloadNotes);
                    }}
                  >
                    edit
                  </button>
                  <button className="link small" onClick={() => void api.editNote(id, n.id).then(data.reloadNotes)}>
                    remove
                  </button>
                </span>
              </div>
              <div className="pre-wrap">
                <Rich text={n.text} />
              </div>
            </div>
          ))}
          {delivered.length > 0 && (
            <details className="small">
              <summary>{delivered.length} note(s) delivered</summary>
              {delivered.map((n) => (
                <div key={n.id} className="delivered">
                  <span className="muted">to {n.to}: </span>
                  <Rich text={n.text} />
                </div>
              ))}
            </details>
          )}
        </section>

        </div>
        <div className="composer-bar">
          <div className="row gap wrap small">
            <label className="muted" htmlFor="note-to">
              Note to
            </label>
            <select id="note-to" value={to} onChange={(e) => setTo(e.target.value)}>
              {recipients.map((r) => (
                <option key={r} value={r}>
                  {r === "workers" ? "every worker" : r}
                </option>
              ))}
            </select>
            <span className="muted">Delivered with the next approval or repair. It does not interrupt a reply.</span>
          </div>
          <Composer
            ref={composer}
            bus
            pages={pages}
            agents={["master", "reviewer", "verifier", ...pages.map((p) => `worker:${p}`)]}
            placeholder="A note for the next brief · @ file, page, agent or key · # pull request · / commands"
            onSend={(m) => void send(m)}
          />
        </div>
      </div>

      {narrow ? (
        <Sheet open={sheet} onClose={() => setSheet(false)}>
          {panel}
        </Sheet>
      ) : (
        <aside className="run-panel">{panel}</aside>
      )}

      {cite && (
        <div ref={citeRef} className="cite-pop" style={{ left: Math.max(8, cite.x - 120), top: Math.max(8, cite.y - 76) }} onMouseUp={(e) => e.stopPropagation()}>
          <input placeholder="Optional comment" value={citeNote} onChange={(e) => setCiteNote(e.target.value)} />
          <div className="row gap">
            <button
              className="primary small"
              onClick={() => {
                const target = cite.agent;
                setTo(target);
                composer.current?.insert({
                  kind: "cite",
                  label: `“${cite.text.slice(0, 28)}${cite.text.length > 28 ? "…" : ""}”`,
                  value: cite.text,
                  to: target,
                  comment: citeNote || undefined,
                });
                setCite(null);
                window.getSelection()?.removeAllRanges();
              }}
            >
              Cite
            </button>
            <button className="ghost small" onClick={() => setCite(null)}>
              ×
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
