import { useEffect, useRef, useState } from "react";
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
  const list = providers.data ?? [];

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
    <div className="page narrow">
      <header className="page-head">
        <h1>New run</h1>
      </header>
      <div className="stack">
        <div className="field">
          <span className="label">What should it build?</span>
          <Composer
            ref={composer}
            bus
            placeholder="A bookshop: a catalogue with search, a page per book, and a cart. Enter sends · Shift+Enter breaks the line"
            onSend={(m) => submit(m)}
          />
          <span className="hint">↑ in an empty field recalls your last prompt · @ file, page, agent or key · #123 a pull request · / commands · drop a file to attach it</span>
        </div>
        <Problem error={action.error} />
        <div className="row gap">
          <button className="primary" disabled={action.busy !== null} onClick={() => submit()}>
            {action.busy ? "Starting…" : "Start planning"}
          </button>
          <label className="check">
            <input type="checkbox" checked={yes} onChange={(e) => setYes(e.target.checked)} />
            Build without asking me to approve the plan
          </label>
        </div>

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
      </div>
    </div>
  );
}
