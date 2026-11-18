import { useEffect, useRef, useState } from "react";
import { ArrowUp, GitMerge, ListChecks, Settings2, Users, X } from "lucide-react";
import { RoleMark } from "../marks";
import { api } from "../api";
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
import { runStarted } from "../notify";

export function NewRun() {
  const providers = useProviders();
  const templates = useLoad(api.templates, []);
  const [template, setTemplate] = useState("");
  const settings = useSettings();
  const max = settings.data?.machine?.max_parallel ?? null;
  const [pages, setPagesState] = useState<Pages>(() => prefs().parallel ?? 3);
  const setPages = (v: Pages) => {
    setPagesState(v);
    setPrefs({ parallel: v });
  };
  const [yes, setYes] = useState(false);
  const [remember, setRemember] = useState(false);
  const { choice, setSeat, missing, fresh } = useChoice(settings.data);
  const composer = useRef<ComposerHandle>(null);
  const action = useAction();
  const [advanced, setAdvanced] = useState(false);
  const list = providers.data ?? [];
  const master = seatChoice(choice, "master");
  const pill = master ? `${list.find((p) => p.id === master.harness)?.name ?? providerName(master.harness)} · ${modelLabel(list, master)}` : "Choose providers";
  const project = settings.data?.project?.split(/[/\\]/).filter(Boolean).pop();
  const parallel = resolvePages(pages, max ?? 32);

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
      const { roles, models } = toRequest(choice);
      const { id } = await api.createRun({ prompt, template: template || undefined, parallel, yes, roles, models });
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
        <p className="ask-sub muted">A master will plan it, then workers build the pages and a reviewer checks the result.</p>
        <Composer
          ref={composer}
          bus
          placeholder="Tell the master what to plan. @ files, # pull requests, / commands"
          onSend={(m) => submit(m)}
          footer={
            <div className="composer-foot">
              <div className="composer-actions">
                <button
                  type="button"
                  className={`model-pill ${advanced ? "on" : ""}`}
                  aria-expanded={advanced}
                  onClick={() => setAdvanced((v) => !v)}
                  title="Roles and pages"
                >
                  <ProviderLogo id={master?.harness} size={14} />
                  <span className="clip">{pill}</span>
                </button>
                <span className="foot-sep" aria-hidden />
                <span className="foot-note">
                  <Users size={13} aria-hidden /> {pages === "max" ? `Max (${parallel})` : parallel} at once
                </span>
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

