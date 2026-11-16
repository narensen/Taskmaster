import { useMemo, useState } from "react";
import { api, DEFAULT_ROLES, type Provider, type RoleName, type Roles } from "../api";
import { navigate } from "../router";
import { Field, Problem, useAction, useLoad } from "../ui";

const ROLES: { id: RoleName; label: string; hint: string }[] = [
  { id: "worker", label: "Worker", hint: "Builds each page first. The cheap one." },
  { id: "mid", label: "Mid", hint: "Takes a page the worker could not finish." },
  { id: "master", label: "Master", hint: "Plans, locks the contracts, and repairs what is left." },
  { id: "reviewer", label: "Reviewer", hint: "Reads the result and reports problems." },
  { id: "verifier", label: "Verifier", hint: "Checks each reported problem. Use a different harness than the reviewer." },
];

export function NewRun() {
  const providers = useLoad(api.providers, []);
  const templates = useLoad(api.templates, []);
  const [prompt, setPrompt] = useState("");
  const [template, setTemplate] = useState("");
  const [parallel, setParallel] = useState(3);
  const [yes, setYes] = useState(false);
  const [roles, setRoles] = useState<Roles>(DEFAULT_ROLES);
  const [models, setModels] = useState<Partial<Record<RoleName, string>>>({});
  const [advanced, setAdvanced] = useState(false);
  const action = useAction();

  const byId = useMemo(() => new Map((providers.data ?? []).map((p) => [p.id, p])), [providers.data]);
  const missing = ROLES.filter((role) => providers.data && !byId.get(roles[role.id])?.installed);

  const submit = (event: React.FormEvent) => {
    event.preventDefault();
    void action.run("create", async () => {
      const chosen = Object.fromEntries(Object.entries(models).filter(([, model]) => model));
      const { id } = await api.createRun({
        prompt: prompt.trim(),
        template: template || undefined,
        parallel,
        yes,
        roles,
        models: chosen,
      });
      navigate(`/runs/${encodeURIComponent(id)}`);
    });
  };

  return (
    <div className="page narrow">
      <header className="page-head">
        <h1>New run</h1>
      </header>
      <form className="stack" onSubmit={submit}>
        <Field label="What should it build?" hint="Name the pages and what each one does. The master turns this into a plan.">
          <textarea
            rows={7}
            value={prompt}
            onChange={(e) => setPrompt(e.target.value)}
            placeholder="A bookshop: a catalogue page with search, a page per book, and a cart."
            autoFocus
          />
        </Field>

        <div className="grid two">
          <Field label="Start from" hint="For a new project. A path to a template directory works too.">
            <input
              list="templates"
              value={template}
              onChange={(e) => setTemplate(e.target.value)}
              placeholder="the project as it is"
              spellCheck={false}
            />
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

        <label className="check">
          <input type="checkbox" checked={yes} onChange={(e) => setYes(e.target.checked)} />
          <span>Build as soon as the plan is ready, without waiting for my approval</span>
        </label>

        <button type="button" className="link" onClick={() => setAdvanced(!advanced)}>
          {advanced ? "Hide" : "Choose"} the harness and model for each role
        </button>

        {advanced && (
          <div className="card stack">
            {providers.loading && <p className="muted">Asking each harness for its models…</p>}
            <Problem error={providers.error} onRetry={providers.reload} />
            {ROLES.map((role) => (
              <RolePicker
                key={role.id}
                role={role}
                providers={providers.data ?? []}
                harness={roles[role.id]}
                model={models[role.id] ?? ""}
                onHarness={(harness) => {
                  setRoles({ ...roles, [role.id]: harness });
                  setModels({ ...models, [role.id]: "" });
                }}
                onModel={(model) => setModels({ ...models, [role.id]: model })}
              />
            ))}
          </div>
        )}

        {missing.length > 0 && (
          <p className="warning">
            Not installed: {[...new Set(missing.map((role) => roles[role.id]))].join(", ")}. See Settings for how to install them.
          </p>
        )}

        <Problem error={action.error} />
        <button className="primary big" disabled={!prompt.trim() || action.busy !== null}>
          {action.busy ? "Starting…" : yes ? "Plan and build" : "Plan it"}
        </button>
      </form>
    </div>
  );
}

function RolePicker({
  role,
  providers,
  harness,
  model,
  onHarness,
  onModel,
}: {
  role: { id: RoleName; label: string; hint: string };
  providers: Provider[];
  harness: string;
  model: string;
  onHarness: (harness: string) => void;
  onModel: (model: string) => void;
}) {
  const provider = providers.find((p) => p.id === harness);
  return (
    <div className="role">
      <div>
        <strong>{role.label}</strong>
        <p className="muted small">{role.hint}</p>
      </div>
      <div className="row gap">
        <select value={harness} onChange={(e) => onHarness(e.target.value)} aria-label={`${role.label} harness`}>
          {providers.length === 0 && <option value={harness}>{harness}</option>}
          {providers.map((p) => (
            <option key={p.id} value={p.id}>
              {p.name}
              {p.installed ? "" : " (not installed)"}
            </option>
          ))}
        </select>
        <select value={model} onChange={(e) => onModel(e.target.value)} aria-label={`${role.label} model`}>
          <option value="">Its default model</option>
          {(provider?.models ?? []).map((m) => (
            <option key={m.id} value={m.id}>
              {m.name}
              {m.default ? " (default)" : ""}
            </option>
          ))}
        </select>
      </div>
    </div>
  );
}
