// First run: no coding CLI is on this machine yet. Instead of a picker with nothing to
// pick, say what Taskmaster needs, how to get each CLI, and where to install it from here.

import { Copy, ExternalLink, RefreshCw } from "lucide-react";
import { api, type Provider } from "../api";
import { toast } from "../overlay";
import { ProviderLogo } from "../providers/logos";
import { keys, queryClient } from "../query";
import { Link } from "../router";

/** Each provider's own install guide. */
const GUIDES: Record<string, string> = {
  claude: "https://code.claude.com/docs/en/setup",
  codex: "https://developers.openai.com/codex/cli",
  grok: "https://x.ai/cli",
  agy: "https://antigravity.google/docs/cli",
  opencode: "https://opencode.ai/docs/",
};

/** True once the list has loaded and nothing in it is installed. */
export function noneInstalled(list: Provider[] | undefined): boolean {
  return !!list && list.length > 0 && list.every((p) => !p.installed);
}

function copy(text: string) {
  void navigator.clipboard?.writeText(text).then(
    () => toast("Install command copied"),
    () => toast("Could not copy the install command"),
  );
}

export function ProviderSetup({ providers, checking }: { providers: Provider[]; checking?: boolean }) {
  return (
    <section className="setup-first" aria-labelledby="setup-first-title" data-testid="provider-setup">
      <h2 id="setup-first-title">Install a coding CLI to start</h2>
      <p className="muted">
        Taskmaster plans and checks the work, and the coding CLIs on this machine write it. None is installed yet. Install at
        least one, sign in to it, then check again. Any of them can play any role.
      </p>
      <ul className="setup-first-list">
        {providers.map((p) => (
          <li key={p.id}>
            <span className="setup-first-name">
              <ProviderLogo id={p.id} size={15} />
              {p.name}
            </span>
            <code className="setup-first-cmd clip" title={p.install}>
              {p.install}
            </code>
            <span className="setup-first-acts">
              <button type="button" className="icon" aria-label={`Copy the ${p.name} install command`} onClick={() => copy(p.install)}>
                <Copy size={13} />
              </button>
              {GUIDES[p.id] && (
                <a className="icon" href={GUIDES[p.id]} target="_blank" rel="noreferrer" aria-label={`${p.name} install guide`}>
                  <ExternalLink size={13} />
                </a>
              )}
            </span>
          </li>
        ))}
      </ul>
      <div className="setup-first-foot">
        <Link to="/providers" className="button small">
          Install from Providers
        </Link>
        <button
          type="button"
          className="ghost small"
          disabled={checking}
          onClick={() => void queryClient.fetchQuery({ queryKey: keys.providers, queryFn: () => api.providers(undefined, true), staleTime: 0 })}
        >
          <RefreshCw size={13} aria-hidden className={checking ? "spin-icon" : ""} /> {checking ? "Checking…" : "Check again"}
        </button>
      </div>
    </section>
  );
}
