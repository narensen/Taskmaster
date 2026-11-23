// A9: on a run that is not working, give a seat to another provider or model. The next
// agent starts fresh with a handoff (the plan, what changed, what still fails) instead of
// the old provider's session.

import * as Popover from "@radix-ui/react-popover";
import { ArrowLeftRight } from "lucide-react";
import { useState } from "react";
import { api, type RunDetail } from "../api";
import { ProviderPicker } from "../components/ProviderPicker";
import { toast } from "../overlay";
import { useProviders } from "../query";
import { SEATS } from "../rolechoice";

export function SwitchProviders({ id, detail, onSwitched }: { id: string; detail: RunDetail; onSwitched: () => void }) {
  const [open, setOpen] = useState(false);
  const providers = useProviders();
  const seats = SEATS.filter((s) => s.id !== "checker");
  const pick = async (seat: (typeof SEATS)[number], harness: string, model: string | null) => {
    const roles = Object.fromEntries(seat.roles.map((r) => [r, harness]));
    const models = Object.fromEntries(seat.roles.map((r) => [r, model ?? ""]));
    try {
      const res = await api.switchRoles(id, { roles, models });
      setOpen(false);
      onSwitched();
      toast(
        res.handoff ? `${seat.label} switched` : "Nothing changed",
        res.handoff ? `The next turn's ${seat.label.toLowerCase()} uses ${harness}${model ? ` · ${model}` : ""} and starts from a handoff.` : undefined,
        5000,
      );
    } catch (err) {
      toast("Not switched", err instanceof Error ? err.message : String(err));
    }
  };
  return (
    <Popover.Root open={open} onOpenChange={setOpen} modal={false}>
      <Popover.Trigger asChild>
        <button type="button" className="model-pill run-opt" title="Give a seat to another provider for the next turn" data-testid="switch-providers">
          <ArrowLeftRight size={12} strokeWidth={1.9} aria-hidden />
          <span>Switch</span>
        </button>
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Content className="run-opt-layer" side="top" align="start" sideOffset={6} collisionPadding={12}>
          <div className="pop run-opt-pop switch-pop" role="dialog" aria-label="Switch providers" data-testid="switch-popover">
            <p className="run-opt-desc">The next turn uses the new provider. It cannot pick up the old one's session, so it starts from a handoff.</p>
            {seats.map((seat) => {
              const role = seat.roles[0]!;
              const value = { harness: detail.roles[role], model: detail.models[role] ?? null };
              return (
                <div key={seat.id} className="switch-seat">
                  <span className="run-opt-title">{seat.label}</span>
                  <ProviderPicker
                    label={`${seat.label} provider for the next turn`}
                    providers={providers.data ?? []}
                    loading={providers.isLoading && !providers.data}
                    value={value}
                    onChange={(p) => void pick(seat, p.harness, p.model)}
                    compact
                  />
                </div>
              );
            })}
          </div>
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}
