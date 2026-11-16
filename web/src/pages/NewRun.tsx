import { useEffect, useRef, useState } from "react";
import { ArrowUp, Sparkles } from "lucide-react";
import { api, DEFAULT_ROLES, type RoleName, type Roles } from "../api";
import { navigate } from "../router";
import { Composer, messageText, type ComposerHandle, type Message } from "../composer";
import { usePrefs } from "../prefs";
import { Field, Problem, useAction, useLoad } from "../ui";
import { ROLES, RolePicker, allowed } from "../roles";

export function NewRun() {
  const providers = useLoad(() => api.providers(), []);
  const templates = useLoad(api.templates, []);
  const prefs = usePrefs();
  const [template, setTemplate] = useState("");
  const [parallel, setParallel] = useState(3);
  const [yes, setYes] = useState(false);
  const [roles, setRoles] = useState<Roles>(() => {
    const r = { ...DEFAULT_ROLES };
    for (const role of ROLES) if (prefs.roleDefaults[role.id]) r[role.id] = prefs.roleDefaults[role.id]!.harness;
    return r;
  });
  const [models, setModels] = useState<Partial<Record<RoleName, string | null>>>(() =>
    Object.fromEntries(ROLES.map((r) => [r.id, prefs.roleDefaults[r.id]?.model ?? null])),
  );
  const composer = useRef<ComposerHandle>(null);
  const action = useAction();
  const [advanced, setAdvanced] = useState(false);
  const list = providers.data ?? [];
  const master = list.find((p) => p.id === roles.master);
  const chosen = master?.models.find((m) => m.id === models.master) ?? master?.models.find((m) => m.default) ?? master?.models[0];
  const modelName = chosen?.name ?? models.master ?? "default";
  const pill = `${master?.name ?? roles.master} · ${modelName}`;

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
        <h1>What should we build?</h1>
        <p className="ask-sub muted">or start from the project as it is</p>
        <Composer
          ref={composer}
          bus
          placeholder="Ask anything. @ files, # pull requests, / commands"
          onSend={(m) => submit(m)}
          footer={
            <div className="composer-foot">
              <div className="composer-actions">
                <button type="button" className="model-pill" onClick={() => setAdvanced((v) => !v)} title="Harnesses and models">
                  <Sparkles size={13} />
                  <span className="clip">{pill}</span>
                </button>
                <button type="button" className="send-btn" aria-label="Start planning" disabled={action.busy !== null} onClick={() => submit()}>
                  <ArrowUp size={15} />
                </button>
              </div>
            </div>
          }
        />
        <div className="row between gap wrap">
          <label className="check small">
            <input type="checkbox" checked={yes} onChange={(e) => setYes(e.target.checked)} />
            Build without asking me to approve the plan
          </label>
          <span className="hint">Enter sends · Shift+Enter breaks the line · ↑ recalls the last prompt</span>
        </div>
        <Problem error={action.error} />
        {advanced && (
          <div className="ask-advanced">
            <h2 className="section-title">Harnesses and models</h2>
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
          </div>
        )}
      </div>
    </div>
  );
}
