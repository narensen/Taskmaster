// Choosing a harness and a model for a role, only from what the providers API returned.

import type { Provider, RoleName } from "./api";

export const ROLES: { id: RoleName; label: string; hint: string }[] = [
  { id: "master", label: "Master", hint: "Plans, locks the contracts, repairs shared files." },
  { id: "worker", label: "Worker", hint: "Builds each page first." },
  { id: "mid", label: "Mid", hint: "Takes a page the worker could not finish." },
  { id: "reviewer", label: "Reviewer", hint: "Reads the result and reports problems." },
  { id: "verifier", label: "Verifier", hint: "Checks each reported problem." },
];

/** Whether a choice is one the list allows. An omitted model is the provider default. */
export function allowed(providers: Provider[], harness: string, model: string | null): string | null {
  const p = providers.find((x) => x.id === harness);
  if (!p) return `\`${harness}\` is not a harness the daemon lists`;
  if (!p.installed) return `${p.name} is not installed`;
  if (model && !p.models.some((m) => m.id === model)) return `\`${model}\` is not a model ${p.name} returned`;
  return null;
}

export function RolePicker({
  role,
  providers,
  harness,
  model,
  onChange,
}: {
  role: { id: RoleName; label: string; hint: string };
  providers: Provider[];
  harness: string;
  model: string | null;
  onChange: (harness: string, model: string | null) => void;
}) {
  const p = providers.find((x) => x.id === harness);
  const problem = providers.length ? allowed(providers, harness, model) : null;
  return (
    <div className="role-pick">
      <div className="role-name">
        <span className="strong">{role.label}</span>
        <span className="muted small">{role.hint}</span>
      </div>
      <select aria-label={`${role.label} harness`} value={harness} onChange={(e) => onChange(e.target.value, null)}>
        {!providers.some((x) => x.id === harness) && <option value={harness}>{harness} (not listed)</option>}
        {providers.map((x) => (
          <option key={x.id} value={x.id} disabled={!x.installed}>
            {x.name}
            {x.installed ? "" : " (not installed)"}
          </option>
        ))}
      </select>
      <select aria-label={`${role.label} model`} value={model ?? ""} onChange={(e) => onChange(harness, e.target.value || null)}>
        <option value="">provider default{p?.models.find((m) => m.default) ? ` (${p.models.find((m) => m.default)!.name})` : ""}</option>
        {p?.models.map((m) => (
          <option key={m.id} value={m.id}>
            {m.name}
          </option>
        ))}
      </select>
      {problem && <div className="error-line small">{problem}</div>}
    </div>
  );
}
