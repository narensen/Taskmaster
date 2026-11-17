import { useEffect, useRef, useState } from "react";
import { ArrowUp, GitMerge, ListChecks, Settings2, Users, X } from "lucide-react";
import { RoleMark } from "../marks";
import { api, DEFAULT_ROLES, type RoleName, type Roles } from "../api";
import { navigate } from "../router";
import { Composer, messageText, type ComposerHandle, type Message } from "../composer";
import { Field, Problem, useAction, useLoad } from "../ui";
import { ROLES, RolePicker, allowed } from "../roles";

export function NewRun() {
  const providers = useLoad(() => api.providers(), []);
  const templates = useLoad(api.templates, []);
  const [template, setTemplate] = useState("");
  const [parallel, setParallel] = useState(3);
  const [yes, setYes] = useState(false);
  const defaults = useLoad(api.settings, []);
  const [roles, setRoles] = useState<Roles>({ ...DEFAULT_ROLES });
  const [models, setModels] = useState<Partial<Record<RoleName, string | null>>>({});
  const [touched, setTouched] = useState(false);
  useEffect(() => {
    if (!defaults.data || touched) return;
    setRoles({ ...DEFAULT_ROLES, ...defaults.data.roles });
    setModels(defaults.data.models ?? {});
  }, [defaults.data, touched]);
  const composer = useRef<ComposerHandle>(null);
  const action = useAction();
  const [advanced, setAdvanced] = useState(false);
  const list = providers.data ?? [];
  const master = list.find((p) => p.id === roles.master);
  const chosen = master?.models.find((m) => m.id === models.master) ?? master?.models.find((m) => m.default) ?? master?.models[0];
  const modelName = chosen?.name ?? models.master ?? "default";
  const pill = `${master?.name ?? roles.master} · ${modelName}`;
  const project = defaults.data?.project?.split(/[/\\]/).filter(Boolean).pop();

  useEffect(() => {
    composer.current?.focus();
  }, []);

  const submit = (message?: Message) => {
    const m = message ?? composer.current?.read();
    if (!m) return;
    const prompt = messageText(m);
    void action.run("create", async () => {
      if (!prompt) throw new Error("Write what to build first.");
      // Only what the providers list returned. An omitted model keeps the default.
      for (const role of ROLES) {
        const problem = allowed(list, roles[role.id], models[role.id] ?? null);
        if (problem) throw new Error(`${role.label}: ${problem}`);
      }
      const chosen = Object.fromEntries(Object.entries(models).filter(([, model]) => model));
      const { id } = await api.createRun({ prompt, template: template || undefined, parallel, yes, roles, models: chosen });
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
                  title="Harnesses and models"
                >
                  <RoleMark role="master" size={14} />
                  <span className="clip">{pill}</span>
                </button>
                <span className="foot-sep" aria-hidden />
                <span className="foot-note">
                  <Users size={13} aria-hidden /> {parallel} at once
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
          <section className="ask-advanced" aria-label="Harnesses and models">
            <div className="row between">
              <h2 className="section-title">Harnesses and models</h2>
              <button type="button" className="icon" aria-label="Close options" onClick={() => setAdvanced(false)}>
                <X size={15} />
              </button>
            </div>
            <Problem error={providers.error} onRetry={providers.reload} />
            {providers.loading && <div className="muted small">Asking each harness for its models…</div>}
            <div className="roles">
              {ROLES.map((role) => (
                <RolePicker
                  key={role.id}
                  role={role}
                  providers={list}
                  harness={roles[role.id]}
                  model={models[role.id] ?? null}
                  onChange={(harness, model) => {
                    setTouched(true);
                    setRoles({ ...roles, [role.id]: harness });
                    setModels({ ...models, [role.id]: model });
                  }}
                />
              ))}
            </div>
            <div className="grid two">
              <Field label="Start from" hint="For a new project. A path to a template directory works too.">
                <input list="templates" value={template} onChange={(e) => setTemplate(e.target.value)} placeholder="the project as it is" spellCheck={false} />
                <datalist id="templates">
                  {(templates.data ?? []).map((name) => (
                    <option key={name} value={name} />
                  ))}
                </datalist>
              </Field>
              <Field label="Pages at once">
                <input type="number" min={1} max={16} value={parallel} onChange={(e) => setParallel(Number(e.target.value) || 1)} />
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

