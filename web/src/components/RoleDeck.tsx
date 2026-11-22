// The four seats of a run, each with its provider and model, changed in place before the
// run starts. A quiet switch keeps the choice for the project.

import { useEffect, useState } from "react";
import { api, type Provider, type ProviderStatus } from "../api";
import { RoleMark } from "../marks";
import { SEATS, problemWith, seatChoice, type Choice, type Seat } from "../rolechoice";
import type { RoleDefault } from "../prefs";
import { ProviderPicker } from "./ProviderPicker";

function harnessLine(provider: ProviderStatus): string {
  if (!provider.installed) return `${provider.name} is not installed.`;
  if (provider.signed_in === true) {
    return provider.account ? `${provider.name} is installed and signed in as ${provider.account}.` : `${provider.name} is installed and signed in.`;
  }
  if (provider.signed_in === false) return `${provider.name} is installed and not signed in.`;
  return `${provider.name} is installed.`;
}

export function RoleDeck({
  choice,
  providers,
  loading,
  onSeat,
  remember,
  onRemember,
  fresh,
  missing,
}: {
  choice: Choice;
  providers: Provider[];
  loading: boolean;
  onSeat: (seat: Seat, pick: RoleDefault) => void;
  remember?: boolean;
  onRemember?: (on: boolean) => void;
  fresh?: boolean;
  missing: Seat[];
}) {
  const [harness, setHarness] = useState<ProviderStatus[] | null>(null);
  useEffect(() => {
    if (!fresh) return;
    let gone = false;
    api.setup().then(
      (report) => {
        if (!gone) setHarness(report.providers);
      },
      () => {
        if (!gone) setHarness([]);
      },
    );
    return () => {
      gone = true;
    };
  }, [fresh]);
  return (
    <section className="role-deck" aria-label="Who plays each role">
      {fresh && missing.length > 0 && (
        <p className="deck-ask">
          Pick a provider for each role. Taskmaster keeps your answer for this project, so you only do this once.
        </p>
      )}
      {fresh && harness && harness.length > 0 && (
        <ul className="deck-harness" data-testid="harness-state">
          {harness.map((provider) => (
            <li key={provider.id}>{harnessLine(provider)}</li>
          ))}
        </ul>
      )}
      <div className="deck-grid">
        {SEATS.map((seat) => {
          const pick = seatChoice(choice, seat.id);
          const split = seat.id === "checker" && choice.reviewer && choice.verifier && (choice.reviewer.harness !== choice.verifier.harness || choice.reviewer.model !== choice.verifier.model);
          const problem = providers.length ? problemWith(providers, pick) : null;
          const role = seat.id === "checker" ? "verifier" : seat.id;
          return (
            <div key={seat.id} className={`seat ${!pick ? "unset" : ""} ${problem && pick ? "bad" : ""}`}>
              <div className="seat-head">
                <RoleMark role={role} size={13} />
                <span className="seat-name">{seat.label}</span>
              </div>
              <ProviderPicker label={`${seat.label} provider`} providers={providers} loading={loading} value={pick} onChange={(p) => onSeat(seat.id, p)} />
              <div className="seat-hint">{split ? "Reviewer and verifier differ. Picking sets both." : problem && pick ? problem : seat.hint}</div>
            </div>
          );
        })}
      </div>
      {onRemember && (
        <label className="quiet-check">
          <input type="checkbox" checked={!!remember} onChange={(e) => onRemember(e.target.checked)} />
          <span>Remember for this project</span>
        </label>
      )}
    </section>
  );
}
