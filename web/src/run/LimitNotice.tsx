// A provider stopped the turn. The sentence is the error. The raw reply stays folded.

import { useState } from "react";
import { api, type RoleName, type RunLimit } from "../api";
import { ProviderPicker } from "../components/ProviderPicker";
import { toast } from "../overlay";
import type { RoleDefault } from "../prefs";
import { useProviders, useSettings } from "../query";
import { rememberChoice, savedChoice } from "../rolechoice";

const ROLES: Record<string, RoleName> = {
  master: "master",
  worker: "worker",
  mid: "mid",
  reviewer: "reviewer",
  verifier: "verifier",
};

export function LimitNotice({ id, limit, message, onDone }: { id: string; limit: RunLimit; message: string; onDone: () => void }) {
  const [raw, setRaw] = useState(false);
  const [switching, setSwitching] = useState(false);
  const [busy, setBusy] = useState(false);
  const providers = useProviders();
  const settings = useSettings();
  const role = ROLES[limit.role.toLowerCase()] ?? "master";
  const saved = savedChoice(settings.data);

  const retry = () => {
    setBusy(true);
    api.resume(id).then(
      () => onDone(),
      (err) => {
        setBusy(false);
        toast("Could not retry", err instanceof Error ? err.message : String(err));
      },
    );
  };

  const save = (pick: RoleDefault) => {
    rememberChoice({ ...saved, [role]: pick }).then(
      () => {
        toast("Saved for this role", `${limit.role}: ${pick.harness}${pick.model ? ` · ${pick.model}` : ""}`);
        setSwitching(false);
      },
      (err) => toast("The choice was not saved", err instanceof Error ? err.message : String(err)),
    );
  };

  return (
    <div className="limit-note" data-testid="limit">
      <p>{message}</p>
      <div className="limit-actions">
        <button type="button" className="small" onClick={() => setSwitching((open) => !open)}>
          Switch provider for this role
        </button>
        <button type="button" className="small" onClick={retry} disabled={busy}>
          Retry
        </button>
        {limit.raw && (
          <button type="button" className="small" onClick={() => setRaw((open) => !open)}>
            {raw ? "Hide the reply" : "Show the reply"}
          </button>
        )}
        {switching && (
          <ProviderPicker
            label={`${limit.role} provider`}
            providers={providers.data ?? []}
            loading={providers.isLoading && !providers.data}
            value={saved[role] ?? null}
            onChange={save}
          />
        )}
      </div>
      {raw && <pre className="step-io">{limit.raw}</pre>}
    </div>
  );
}
