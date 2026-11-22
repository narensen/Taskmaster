import { useEffect, useMemo, useRef, useState } from "react";
import {
  AlertTriangle,
  ArrowUp,
  CheckCircle2,
  Coins,
  GitMerge,
  ListChecks,
  Settings2,
  ShieldAlert,
  Sparkles,
  Users,
  X,
  Zap,
} from "lucide-react";
import { RoleMark } from "../marks";
import { api, type DoctorReport } from "../api";
import { navigate } from "../router";
import { Composer, messageText, type ComposerHandle, type Message } from "../composer";
import { Field, Problem, useAction, useLoad } from "../ui";
import { RoleDeck } from "../components/RoleDeck";
import { PagesPicker, resolvePages, type Pages } from "../components/PagesPicker";
import { ProviderLogo, providerName } from "../providers/logos";
import { modelLabel } from "../components/ProviderPicker";
import { afterStart, problemWith, SEATS, seatChoice, toRequest, useChoice } from "../rolechoice";
import { prefs, setPrefs } from "../prefs";
import { useProviders, useSettings } from "../query";
import { ProviderSetup, noneInstalled } from "../components/ProviderSetup";
import { runStarted } from "../notify";

const BUDGET_PRESETS = [
  { label: "Unlimited", value: "" },
  { label: "$0.50", value: "0.5" },
  { label: "$2", value: "2" },
  { label: "$5", value: "5" },
  { label: "$15", value: "15" },
];

export function NewRun() {
  const providers = useProviders();
  const templates = useLoad(api.templates, []);
  const doctor = useLoad(api.doctor, []);
  const [doctorOpen, setDoctorOpen] = useState(false);
  const [template, setTemplate] = useState("");
  const settings = useSettings();
  const max = settings.data?.machine?.max_parallel ?? null;
  const [pages, setPagesState] = useState<Pages | null>(() => prefs().parallel);
  const setPages = (v: Pages) => {
    setPagesState(v);
    setPrefs({ parallel: v });
  };
  const [yes, setYes] = useState(false);
  const [remember, setRemember] = useState(false);
  const [budgetCap, setBudgetCap] = useState<string>("");
  const { choice, setSeat, missing, fresh } = useChoice(settings.data);
  const composer = useRef<ComposerHandle>(null);
  const action = useAction();
  const [advanced, setAdvanced] = useState(false);
  const list = providers.data ?? [];
  const master = seatChoice(choice, "master");
  const worker = seatChoice(choice, "worker");
  const masterName = master ? (list.find((p) => p.id === master.harness)?.name ?? providerName(master.harness)) : null;
  const workerName = worker ? (list.find((p) => p.id === worker.harness)?.name ?? providerName(worker.harness)) : null;
  const pill = master ? `Master · ${masterName} · ${modelLabel(list, master)}` : "Choose providers";
  const workersLabel =
    pages == null
      ? workerName
        ? `Workers · ${workerName}`
        : "Choose workers"
      : `${pages === "max" ? `Max (${max ?? "…"})` : `${pages} worker${pages === 1 ? "" : "s"}`}${workerName ? ` · ${workerName}` : ""}`;
  const project = settings.data?.project?.split(/[/\\]/).filter(Boolean).pop();

  const doctorReport: DoctorReport | null = doctor.data ?? null;
  const doctorTone = useMemo(() => {
    if (!doctorReport) return "muted";
    if (!doctorReport.ok) return "rose";
    if (doctorReport.checks.some((c) => c.status === "warn")) return "amber";
    return "emerald";
  }, [doctorReport]);

  useEffect(() => {
    composer.current?.focus();
  }, []);

  // A role with no provider opens the options, so the question is asked where it is answered.
  useEffect(() => {
    if (settings.data && missing.length) setAdvanced(true);
  }, [settings.data, missing.length]);

  const submit = (message?: Message) => {
    const m = message ?? composer.current?.read();
    if (!m) return;
    const prompt = messageText(m);
    void action.run("create", async () => {
      if (!prompt) throw new Error("Write what to build first.");
      if (missing.length) {
        setAdvanced(true);
        throw new Error(`Choose a provider for ${missing.map((s) => SEATS.find((x) => x.id === s)!.label.toLowerCase()).join(", ")}.`);
      }
      for (const seat of SEATS) {
        const problem = problemWith(list, seatChoice(choice, seat.id));
        if (problem) throw new Error(`${seat.label}: ${problem}`);
      }
      if (pages == null) {
        setAdvanced(true);
        throw new Error("Choose how many pages to run at once.");
      }
      const { roles, models } = toRequest(choice);
      const parallel = resolvePages(pages, max ?? 32);
      const parsedCap = budgetCap.trim() ? Number(budgetCap.trim()) : undefined;
      const max_cost_usd = parsedCap && Number.isFinite(parsedCap) && parsedCap > 0 ? parsedCap : undefined;
      const { id } = await api.createRun({ prompt, template: template || undefined, parallel, yes, roles, models, max_cost_usd });
      void afterStart(choice, settings.data, remember);
      runStarted(id);
      composer.current?.clear();
      navigate(`/runs/${encodeURIComponent(id)}`);
    });
  };

  return (
    <div className="page ask-page">
      <div className="ask-hero">
        <div className="ask-mark" aria-hidden>
          <img src="/icon.png" alt="" width={36} height={36} />
        </div>
        <h1>
          Describe the next run{project ? <> for <span className="ask-project">{project}</span></> : null}
        </h1>
        <p className="ask-sub muted">A master plans it, workers build the pages, and a different model checks the result.</p>

        {/* Live Preflight Doctor + Mode Telemetry Strip */}
        <div className="ask-preflight-row">
          {doctorReport && (
            <button
              type="button"
              className={`doctor-pill tone-${doctorTone}`}
              onClick={() => setDoctorOpen((v) => !v)}
              aria-expanded={doctorOpen}
              data-testid="doctor-pill"
            >
              {doctorTone === "emerald" ? (
                <CheckCircle2 size={13} aria-hidden />
              ) : doctorTone === "rose" ? (
                <ShieldAlert size={13} aria-hidden />
              ) : (
                <AlertTriangle size={13} aria-hidden />
              )}
              <span>{doctorReport.summary}</span>
            </button>
          )}
          <span className="studio-pill tone-indigo" title="Scoped bugfixes auto-route to 0-planner Direct Fast-Path; larger prompts build a parallel DAG.">
            <Zap size={12} aria-hidden /> Auto Fast-Path / Parallel DAG
          </span>
          <span className="studio-pill tone-cyan" title="Shared prefix caching + Code Mode sandboxes enabled">
            <Sparkles size={12} aria-hidden /> Code Mode v2
          </span>
        </div>

        {doctorOpen && doctorReport && (
          <section className="doctor-drawer" aria-label="Preflight readiness checks" data-testid="doctor-drawer">
            <div className="row between">
              <strong>Preflight Doctor</strong>
              <button type="button" className="icon" onClick={() => setDoctorOpen(false)} aria-label="Close doctor">
                <X size={14} />
              </button>
            </div>
            <ul className="doctor-list">
              {doctorReport.checks.map((c) => (
                <li key={c.id} className={`doctor-item st-${c.status}`}>
                  <span className={`doctor-dot st-${c.status}`} aria-hidden />
                  <div className="doctor-item-copy">
                    <div className="row gap">
                      <strong>{c.label}</strong>
                      <span className="muted small mono">{c.category}</span>
                    </div>
                    <div className="small muted">{c.detail}</div>
                    {c.fix && <div className="doctor-fix small">{c.fix}</div>}
                  </div>
                </li>
              ))}
            </ul>
          </section>
        )}

        <Composer
          ref={composer}
          bus
          placeholder="Tell the master what to plan. @ files, # pull requests, $ skills, / commands"
          onSend={(m) => submit(m)}
          footer={
            <div className="composer-foot">
              <div className="composer-actions">
                <button
                  type="button"
                  className={`model-pill ${advanced ? "on" : ""}`}
                  aria-expanded={advanced}
                  onClick={() => setAdvanced((v) => !v)}
                  title="Master provider and roles"
                >
                  <ProviderLogo id={master?.harness} size={14} />
                  <span className="clip">{pill}</span>
                </button>
                <span className="foot-sep" aria-hidden />
                <button
                  type="button"
                  className={`model-pill composer-worker-pill ${advanced ? "on" : ""}`}
                  aria-expanded={advanced}
                  onClick={() => setAdvanced((v) => !v)}
                  title="Worker pool provider and parallel count"
                >
                  <Users size={13} aria-hidden />
                  {worker?.harness && <ProviderLogo id={worker.harness} size={13} />}
                  <span className="clip">{workersLabel}</span>
                </button>
                <span className="foot-sep" aria-hidden />
                <label className="model-pill budget-select-pill" title="Maximum USD spend cap for this run">
                  <Coins size={13} aria-hidden />
                  <select
                    aria-label="Run budget cap"
                    value={budgetCap}
                    onChange={(e) => setBudgetCap(e.target.value)}
                  >
                    {BUDGET_PRESETS.map((b) => (
                      <option key={b.label} value={b.value}>
                        Cap: {b.label}
                      </option>
                    ))}
                  </select>
                </label>
              </div>
              <button type="button" className="send-btn" aria-label="Start planning" disabled={action.busy !== null} onClick={() => submit()}>
                {action.busy ? <span className="spin" aria-hidden /> : <ArrowUp size={16} />}
              </button>
            </div>
          }
        />
        <div className="ask-options">
          <label className={`toggle-chip ${yes ? "on" : ""}`}>
            <input type="checkbox" checked={yes} onChange={(e) => setYes(e.target.checked)} />
            Build without asking me to approve the plan
          </label>
          <button type="button" className="ghost small" onClick={() => setAdvanced((v) => !v)} aria-expanded={advanced}>
            <Settings2 size={13} aria-hidden /> Options
          </button>
          <span className="hint ask-keys">
            <kbd>Enter</kbd> sends · <kbd>Shift</kbd>+<kbd>Enter</kbd> new line · <kbd>↑</kbd> last prompt
          </span>
        </div>
        <Problem error={action.error} />
        {noneInstalled(providers.data) && <ProviderSetup providers={list} checking={providers.isFetching} />}
        {advanced && (
          <section className="ask-advanced" aria-label="Roles and pages">
            <div className="row between">
              <h2 className="section-title">Roles and pages</h2>
              <button type="button" className="icon" aria-label="Close options" onClick={() => setAdvanced(false)}>
                <X size={15} />
              </button>
            </div>
            <Problem error={providers.error} onRetry={() => void providers.refetch()} />
            <RoleDeck
              choice={choice}
              providers={list}
              loading={providers.isLoading}
              onSeat={setSeat}
              remember={remember}
              onRemember={setRemember}
              fresh={fresh}
              missing={missing}
            />
            <PagesPicker value={pages} onChange={setPages} max={max} />
            <div className="grid two">
              <Field label="Start from" hint="For a new project. A folder with a template works too.">
                <input list="templates" value={template} onChange={(e) => setTemplate(e.target.value)} placeholder="the project as it is" spellCheck={false} />
                <datalist id="templates">
                  {(templates.data ?? []).map((name) => (
                    <option key={name} value={name} />
                  ))}
                </datalist>
              </Field>
              <Field label="Custom budget cap (USD)" hint="Stops the run gracefully once spend reaches this ceiling.">
                <input
                  type="number"
                  min="0.1"
                  step="0.5"
                  className="mono"
                  value={budgetCap}
                  onChange={(e) => setBudgetCap(e.target.value)}
                  placeholder="unlimited"
                />
              </Field>
            </div>
            <div>
              <button className="primary" disabled={action.busy !== null} onClick={() => submit()}>
                {action.busy ? "Starting…" : "Start planning"}
              </button>
            </div>
          </section>
        )}
        {!advanced && (
          <>
            <div className="ask-ideas" aria-label="Starting points">
              {IDEAS.map((idea) => (
                <button key={idea} type="button" className="idea" onClick={() => composer.current?.setText(idea)}>
                  {idea}
                </button>
              ))}
            </div>
            <ol className="ask-steps" aria-label="How a run goes">
              <li>
                <ListChecks size={15} aria-hidden />
                <span><strong>Plan</strong> pages, contracts and a design spec you approve</span>
              </li>
              <li>
                <RoleMark role="worker" size={15} />
                <span><strong>Build</strong> in parallel, one agent per page</span>
              </li>
              <li>
                <RoleMark role="verifier" size={15} />
                <span><strong>Check</strong> with free static and browser checks</span>
              </li>
              <li>
                <GitMerge size={15} aria-hidden />
                <span><strong>Merge</strong> into your branch, review on demand</span>
              </li>
            </ol>
          </>
        )}
      </div>
    </div>
  );
}

const IDEAS = [
  "A landing page with a hero, three feature cards and a signup form",
  "Add a settings page with a dark mode toggle",
  "A small dashboard with a table and a chart",
];

