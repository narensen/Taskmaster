import { useEffect, useMemo, useRef, useState, type FormEvent } from "react";
import { ArrowUp, ArrowDown, Check, CircleAlert, CircleCheck, CircleDashed, CircleArrowUp, Copy, ExternalLink, RefreshCw, Square } from "lucide-react";
import {
  api,
  followJob,
  type Provider,
  type ProviderStatus,
  type RuntimeStatus,
  type SetupAction,
  type SetupJob,
  type TerminalKey,
} from "../api";
import { toast } from "../overlay";
import { Loading, Problem, useLoad } from "../ui";

const ACTION_DONE: Record<SetupAction, string> = { install: "Installed", update: "Updated", login: "Signed in" };

function copy(text: string, what: string) {
  void navigator.clipboard?.writeText(text).then(
    () => toast(`${what} copied`),
    () => toast(`Could not copy the ${what.toLowerCase()}`),
  );
}

/** Follow a job's stream while it runs. Calls `settled` once when it stops. */
function useJob(initial: SetupJob | undefined, settled: (job: SetupJob) => void) {
  const [job, setJob] = useState<SetupJob | undefined>(initial);
  const told = useRef<string | null>(null);
  const onSettled = useRef(settled);
  onSettled.current = settled;
  useEffect(() => setJob(initial), [initial?.id]); // eslint-disable-line react-hooks/exhaustive-deps
  const id = job?.id;
  const running = job?.state === "running";
  useEffect(() => {
    if (!id || !running) return;
    return followJob(
      id,
      (next) => {
        setJob(next);
        if (next.state !== "running" && told.current !== next.id) {
          told.current = next.id;
          onSettled.current(next);
        }
      },
      () => void api.job(id).then(setJob, () => undefined),
    );
  }, [id, running]);
  return [job, setJob] as const;
}

function StateLine({ job }: { job: SetupJob }) {
  const current = job.steps.find((s) => s.state === "running") ?? job.steps.find((s) => s.state === "failed");
  if (job.state === "running")
    return (
      <span className="job-state">
        <span className="spin" aria-hidden /> {current?.label ?? "Starting"}
      </span>
    );
  if (job.state === "done") return <span className="job-state good">{ACTION_DONE[job.action]}</span>;
  if (job.state === "cancelled") return <span className="job-state">Stopped</span>;
  return <span className="job-state bad">{job.error ?? "Failed"}</span>;
}

function JobPanel({ job, onClose }: { job: SetupJob; onClose: () => void }) {
  const out = useRef<HTMLPreElement>(null);
  const [typed, setTyped] = useState("");
  const [sending, setSending] = useState(false);
  useEffect(() => {
    const el = out.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [job.output]);
  const send = async (body: { text?: string; key?: TerminalKey }) => {
    setSending(true);
    try {
      await api.jobInput(job.id, body);
    } catch (err) {
      toast("Not sent", err instanceof Error ? err.message : String(err));
    } finally {
      setSending(false);
    }
  };
  const submit = (e: FormEvent) => {
    e.preventDefault();
    const text = typed;
    setTyped("");
    void send(text ? { text, key: "enter" } : { key: "enter" });
  };
  const login = job.action === "login";
  return (
    <div className="job" data-testid="job" aria-label={`${job.action} ${job.target}`}>
      <div className="job-head">
        <StateLine job={job} />
        <span className="grow" />
        {job.state === "running" ? (
          <button className="ghost small" onClick={() => void api.cancelJob(job.id)}>
            <Square size={12} aria-hidden /> Stop
          </button>
        ) : (
          <button className="ghost small" onClick={onClose}>
            Hide
          </button>
        )}
      </div>

      {(job.prompt.url || job.prompt.code) && job.state === "running" && (
        <div className="sign-in-prompt" role="group" aria-label="Sign-in details">
          {job.prompt.code && (
            <div className="device-code">
              <span className="muted small">Code</span>
              <code className="device-code-value" data-testid="device-code">
                {job.prompt.code}
              </code>
              <button className="icon" aria-label="Copy the code" title="Copy the code" onClick={() => copy(job.prompt.code!, "Code")}>
                <Copy size={14} aria-hidden />
              </button>
            </div>
          )}
          {job.prompt.url && (
            <div className="sign-in-url">
              <a className="button small" href={job.prompt.url} target="_blank" rel="noreferrer noopener">
                <ExternalLink size={13} aria-hidden /> Open the sign-in page
              </a>
              <code className="sign-in-url-text" title={job.prompt.url}>
                {job.prompt.url}
              </code>
              <button className="icon" aria-label="Copy the address" title="Copy the address" onClick={() => copy(job.prompt.url!, "Address")}>
                <Copy size={14} aria-hidden />
              </button>
            </div>
          )}
        </div>
      )}

      <ol className="job-steps">
        {job.steps.map((s, i) => (
          <li key={i} className={`job-step ${s.state}`}>
            <span className="job-step-mark" aria-hidden>
              {s.state === "done" ? <Check size={12} /> : s.state === "failed" ? <CircleAlert size={12} /> : s.state === "running" ? <span className="spin" /> : <CircleDashed size={12} />}
            </span>
            <span>{s.label}</span>
            <code className="muted job-step-cmd" title={s.command}>
              {s.command.split("\n")[0]}
            </code>
            <span className="sr-only">{s.state}</span>
          </li>
        ))}
      </ol>

      <pre ref={out} className="job-output" tabIndex={0} aria-label="Output">
        {job.output || "Waiting for output"}
      </pre>

      {login && job.accepts_input && (
        <form className="job-input" onSubmit={submit}>
          <label className="sr-only" htmlFor={`type-${job.id}`}>
            Type into the sign-in screen
          </label>
          <input
            id={`type-${job.id}`}
            value={typed}
            onChange={(e) => setTyped(e.target.value)}
            placeholder="Paste a code or answer the prompt"
            autoComplete="off"
            spellCheck={false}
            maxLength={512}
          />
          <button type="submit" className="small" disabled={sending}>
            Send
          </button>
          <span className="job-keys" role="group" aria-label="Keys">
            <button type="button" className="icon" aria-label="Up" title="Up" onClick={() => void send({ key: "up" })}>
              <ArrowUp size={14} aria-hidden />
            </button>
            <button type="button" className="icon" aria-label="Down" title="Down" onClick={() => void send({ key: "down" })}>
              <ArrowDown size={14} aria-hidden />
            </button>
            <button type="button" className="ghost small" onClick={() => void send({ key: "escape" })}>
              Esc
            </button>
          </span>
        </form>
      )}
    </div>
  );
}

function StatusRow({ status }: { status: ProviderStatus | undefined }) {
  if (!status) return <div className="provider-status muted small">Checking…</div>;
  const signIn =
    status.signed_in === true ? (
      <span className="good">
        <CircleCheck size={13} aria-hidden /> Signed in{status.account ? ` as ${status.account}` : ""}
      </span>
    ) : status.signed_in === false ? (
      <span className="ask">
        <CircleDashed size={13} aria-hidden /> Not signed in
      </span>
    ) : (
      <span className="muted">
        <CircleDashed size={13} aria-hidden /> Sign-in not reported
      </span>
    );
  const update =
    status.update_available === true ? (
      <span className="ask">
        <CircleArrowUp size={13} aria-hidden /> {status.latest} available
      </span>
    ) : status.update_available === false ? (
      <span className="muted">Up to date</span>
    ) : (
      <span className="muted">Newest version not published</span>
    );
  return (
    <ul className="provider-status small" aria-label={`${status.name} status`}>
      <li>
        {status.installed ? (
          <span className="good">
            <CircleCheck size={13} aria-hidden /> Installed{status.version ? ` ${status.version}` : ""}
          </span>
        ) : (
          <span className="muted">
            <CircleDashed size={13} aria-hidden /> Not installed
          </span>
        )}
      </li>
      {status.installed && <li>{signIn}</li>}
      {status.installed && <li>{update}</li>}
    </ul>
  );
}

function ProviderCard({
  id,
  name,
  status,
  models,
  job: latest,
  onChanged,
}: {
  id: string;
  name: string;
  status: ProviderStatus | undefined;
  models: Provider | undefined;
  job: SetupJob | undefined;
  onChanged: () => void;
}) {
  const [hidden, setHidden] = useState<string | null>(null);
  const [job, setJob] = useJob(latest, (done) => {
    if (done.state === "done") toast(`${name}: ${ACTION_DONE[done.action].toLowerCase()}`);
    onChanged();
  });
  const busy = job?.state === "running";
  const start = async (action: SetupAction) => {
    try {
      const started = await api.startProviderJob(id, action);
      setHidden(null);
      setJob(started);
    } catch (err) {
      toast(`Could not ${action === "login" ? "sign in" : action}`, err instanceof Error ? err.message : String(err));
    }
  };
  const installed = status?.installed ?? models?.installed ?? false;
  const def = models?.models.find((m) => m.default);
  return (
    <section className={`card provider ${installed ? "" : "missing"}`} data-testid="provider" aria-labelledby={`p-${id}`}>
      <header className="provider-head">
        <span className="grow">
          <span className="strong" id={`p-${id}`}>
            {name}
          </span>{" "}
          <span className="muted">{id}</span>
        </span>
        <span className="provider-actions">
          {!installed && (
            <button className="primary small" disabled={busy || !status} onClick={() => void start("install")}>
              Install
            </button>
          )}
          {installed && status && status.update_available !== false && (
            <button className="small" disabled={busy} onClick={() => void start("update")} title={status.update_available ? `Update to ${status.latest}` : "Run the CLI's own update"}>
              Update
            </button>
          )}
          {installed && (
            <button className={status?.signed_in === false ? "primary small" : "small"} disabled={busy} onClick={() => void start("login")}>
              {status?.signed_in ? "Sign in again" : "Sign in"}
            </button>
          )}
        </span>
      </header>
      <StatusRow status={status} />
      {job && hidden !== job.id && <JobPanel job={job} onClose={() => setHidden(job.id)} />}
      {status?.notes.length ? (
        <ul className="provider-notes small muted">
          {status.notes.map((n) => (
            <li key={n}>{n}</li>
          ))}
        </ul>
      ) : null}
      {models?.error && <div className="error-line small">{models.error}</div>}
      <details className="provider-more">
        <summary>
          {models ? (models.models.length ? `${models.models.length} models${def ? `, ${def.name} by default` : ""}` : "No models returned") : "Models"}
          <span className="muted"> · commands</span>
        </summary>
        <dl className="facts small">
          <dt>install</dt>
          <dd>
            <code>{models?.install ?? "…"}</code>
          </dd>
          <dt>sign in</dt>
          <dd>
            <code>{models?.login ?? "…"}</code>
          </dd>
          {status?.path && (
            <>
              <dt>path</dt>
              <dd>
                <code>{status.path}</code>
              </dd>
            </>
          )}
        </dl>
        {models && models.models.length > 0 && (
          <ul className="model-list small">
            {models.models.map((m) => (
              <li key={m.id} className={m.default ? "strong" : ""}>
                {m.name} <span className="muted">{m.id}</span>
                {m.default && " · default"}
              </li>
            ))}
          </ul>
        )}
      </details>
    </section>
  );
}

function RuntimeRow({ rt, job: latest, onChanged }: { rt: RuntimeStatus; job: SetupJob | undefined; onChanged: () => void }) {
  const [hidden, setHidden] = useState<string | null>(null);
  const [job, setJob] = useJob(latest, (done) => {
    if (done.state === "done") toast(`${rt.name} installed`);
    onChanged();
  });
  const install = async () => {
    try {
      setJob(await api.installRuntime(rt.id));
      setHidden(null);
    } catch (err) {
      toast(`Could not install ${rt.name}`, err instanceof Error ? err.message : String(err));
    }
  };
  return (
    <li className="runtime" data-testid="runtime">
      <div className="runtime-row">
        <span className="strong">{rt.name}</span>
        {rt.installed ? (
          <span className="good small">
            <CircleCheck size={13} aria-hidden /> {rt.version ?? "installed"}
          </span>
        ) : (
          <span className="muted small">
            <CircleDashed size={13} aria-hidden /> Not installed
          </span>
        )}
        <span className="muted small grow">{rt.used_for}</span>
        {!rt.installed && (
          <button className="small" disabled={job?.state === "running"} onClick={() => void install()}>
            Install
          </button>
        )}
      </div>
      {job && hidden !== job.id && <JobPanel job={job} onClose={() => setHidden(job.id)} />}
    </li>
  );
}

const ORDER = ["claude", "grok", "agy", "codex", "opencode"];

export function Providers() {
  const setup = useLoad(() => api.setup(), []);
  const models = useLoad(() => api.providers(), []);
  const reload = () => {
    void setup.reload();
    void models.reload();
  };
  const ids = useMemo(() => {
    const known = new Set([...(setup.data?.providers ?? []).map((p) => p.id), ...(models.data ?? []).map((p) => p.id)]);
    return ORDER.filter((id) => known.has(id) || (!setup.data && !models.data));
  }, [setup.data, models.data]);
  const jobFor = (kind: string, target: string) => setup.data?.jobs.find((j) => j.kind === kind && j.target === target);
  const loading = setup.loading && !setup.data && models.loading && !models.data;
  return (
    <div className="page">
      <header className="page-head">
        <div className="grow">
          <h1>Providers</h1>
          <p className="page-lede">
            The harness CLIs on the project machine. Install, update and sign in from here: the daemon runs each provider's own installer and sign-in, and nothing typed on this page becomes a command.
          </p>
        </div>
        <button onClick={reload} disabled={setup.loading}>
          <RefreshCw size={14} aria-hidden /> Check again
        </button>
      </header>

      <Problem error={setup.error ?? models.error} onRetry={reload} />
      {loading ? (
        <div className="providers">
          <Loading rows={1} tall label="Checking each provider" />
          <Loading rows={1} tall label="Checking each provider" />
        </div>
      ) : (
        <div className="providers">
          {ids.map((id) => {
            const status = setup.data?.providers.find((p) => p.id === id);
            const listed = models.data?.find((p) => p.id === id);
            return (
              <ProviderCard
                key={id}
                id={id}
                name={status?.name ?? listed?.name ?? id}
                status={status}
                models={listed}
                job={jobFor("provider", id)}
                onChanged={reload}
              />
            );
          })}
        </div>
      )}

      <section className="runtimes-section" aria-labelledby="runtimes-h">
        <h2 id="runtimes-h">Runtimes</h2>
        <p className="muted small">The five CLIs are self-contained programs. Builds, checks, skills and MCP servers still start these, so install the ones a project needs.</p>
        {setup.data ? (
          <ul className="runtimes card">
            {setup.data.runtimes.map((rt) => (
              <RuntimeRow key={rt.id} rt={rt} job={jobFor("runtime", rt.id)} onChanged={() => void setup.reload()} />
            ))}
          </ul>
        ) : (
          <Loading rows={3} label="Checking runtimes" />
        )}
      </section>
    </div>
  );
}
