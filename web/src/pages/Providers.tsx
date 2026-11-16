import { api } from "../api";
import { Problem, Spinner, useLoad } from "../ui";

export function Providers() {
  const providers = useLoad(() => api.providers(), []);
  return (
    <div className="page">
      <header className="page-head">
        <h1>Providers</h1>
        <button className="ghost" onClick={() => void providers.reload()}>
          Ask again
        </button>
      </header>
      <p className="muted small">What each harness on the project machine says right now. New Run only offers these harnesses and models.</p>
      <Problem error={providers.error} onRetry={providers.reload} />
      {providers.loading && !providers.data && <Spinner label="Asking each harness for its models" />}
      <div className="providers">
        {(providers.data ?? []).map((p) => {
          const def = p.models.find((m) => m.default);
          return (
            <section key={p.id} className={`card provider ${p.installed ? "" : "missing"}`} data-testid="provider">
              <header className="row between gap">
                <span>
                  <span className="strong">{p.name}</span> <span className="muted">{p.id}</span>
                </span>
                <span className={p.installed ? "strong" : "muted"}>{p.installed ? "installed" : "not installed"}</span>
              </header>
              <dl className="facts small">
                <dt>install</dt>
                <dd>
                  <code>{p.install}</code>
                </dd>
                <dt>login</dt>
                <dd>
                  <code>{p.login}</code>
                </dd>
                <dt>default</dt>
                <dd>{def ? `${def.name} (${def.id})` : "none returned"}</dd>
                <dt>models</dt>
                <dd>{p.models.length ? `${p.models.length} returned` : "none returned"}</dd>
              </dl>
              {p.error && <div className="error-line small">{p.error}</div>}
              {p.models.length > 0 && (
                <ul className="model-list small">
                  {p.models.map((m) => (
                    <li key={m.id} className={m.default ? "strong" : ""}>
                      {m.name} <span className="muted">{m.id}</span>
                      {m.default && " · default"}
                    </li>
                  ))}
                </ul>
              )}
            </section>
          );
        })}
      </div>
    </div>
  );
}
