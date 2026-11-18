// The four seats of a run, each with its provider and model, changed in place before the
// run starts. A quiet switch keeps the choice for the project.

import { AnimatePresence, motion } from "motion/react";
import type { Provider } from "../api";
import { RoleMark } from "../marks";
import { SEATS, problemWith, seatChoice, type Choice, type Seat } from "../rolechoice";
import type { RoleDefault } from "../prefs";
import { bezier, dur } from "../motion";
import { ProviderPicker } from "./ProviderPicker";

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
  return (
    <section className="role-deck" aria-label="Who plays each role">
      <AnimatePresence initial={false}>
        {fresh && missing.length > 0 && (
          <motion.p
            className="deck-ask"
            initial={{ opacity: 0, y: -4 }}
            animate={{ opacity: 1, y: 0 }}
            exit={{ opacity: 0 }}
            transition={{ duration: dur.base, ease: bezier.out }}
          >
            Pick a provider for each role. Taskmaster keeps your answer for this project, so you only do this once.
          </motion.p>
        )}
      </AnimatePresence>
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
