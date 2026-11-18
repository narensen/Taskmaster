// Choose a provider and a model for a role, right where the run starts. Only what the
// providers API returned is offered; a provider that is not installed is shown but cannot
// be picked.

import * as Popover from "@radix-ui/react-popover";
import { motion } from "motion/react";
import { Check, ChevronDown } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { Provider } from "../api";
import type { RoleDefault } from "../prefs";
import { ProviderLogo, providerName } from "../providers/logos";
import { bezier, dur } from "../motion";
import { useMotion } from "../prefs";
import { rove } from "./roving";

export function modelLabel(providers: Provider[], pick: RoleDefault | null): string {
  if (!pick) return "";
  const p = providers.find((x) => x.id === pick.harness);
  if (pick.model) return p?.models.find((m) => m.id === pick.model)?.name ?? pick.model;
  const def = p?.models.find((m) => m.default);
  return def ? `${def.name}` : "default model";
}

export function ProviderPicker({
  label,
  providers,
  loading,
  value,
  onChange,
  compact = false,
}: {
  /** What the picker is for, read by a screen reader: "Master provider". */
  label: string;
  providers: Provider[];
  loading?: boolean;
  value: RoleDefault | null;
  onChange: (pick: RoleDefault) => void;
  compact?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const [browse, setBrowse] = useState<string | null>(value?.harness ?? null);
  const on = useMotion();
  const models = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (open) setBrowse(value?.harness ?? providers.find((p) => p.installed)?.id ?? null);
  }, [open, value?.harness, providers]);
  const current = providers.find((p) => p.id === browse);
  const chosen = value ? providers.find((p) => p.id === value.harness) : undefined;
  const name = value ? (chosen?.name ?? providerName(value.harness)) : null;

  const pickProvider = (p: Provider) => {
    setBrowse(p.id);
    if (value?.harness !== p.id) onChange({ harness: p.id, model: null });
    requestAnimationFrame(() => models.current?.querySelector<HTMLElement>("[data-rove]")?.focus());
  };

  return (
    <Popover.Root open={open} onOpenChange={setOpen}>
      <Popover.Trigger asChild>
        <button type="button" className={`prov-trigger ${value ? "" : "unset"} ${compact ? "compact" : ""}`} aria-label={`${label}: ${name ? `${name}, ${modelLabel(providers, value)}` : "not chosen"}`}>
          <ProviderLogo id={value?.harness} size={compact ? 13 : 14} />
          {value ? (
            <span className="prov-trigger-text">
              <span className="prov-trigger-name">{name}</span>
              {!compact && <span className="prov-trigger-model">{modelLabel(providers, value)}</span>}
            </span>
          ) : (
            <span className="prov-trigger-text">
              <span className="prov-trigger-name">Choose</span>
            </span>
          )}
          <ChevronDown size={12} className="prov-chev" aria-hidden />
        </button>
      </Popover.Trigger>
      {open && (
          <Popover.Portal>
            <Popover.Content asChild sideOffset={6} align="start" collisionPadding={12}>
              <motion.div
                className="pop prov-pop"
                initial={on ? { opacity: 0, y: -4, scale: 0.98 } : false}
                animate={{ opacity: 1, y: 0, scale: 1 }}
                transition={{ duration: dur.fast, ease: bezier.out }}
              >
                <div className="prov-cols">
                  <div className="prov-list" role="listbox" aria-label="Providers" onKeyDown={(e) => rove(e)}>
                    <div className="pop-label">Provider</div>
                    {loading && !providers.length && (
                      <>
                        <span className="skeleton prov-skel" />
                        <span className="skeleton prov-skel" />
                        <span className="skeleton prov-skel" />
                      </>
                    )}
                    {providers.map((p) => (
                      <button
                        key={p.id}
                        type="button"
                        role="option"
                        data-rove
                        aria-selected={value?.harness === p.id}
                        disabled={!p.installed}
                        className={`pop-item ${browse === p.id ? "browsing" : ""}`}
                        onMouseEnter={() => p.installed && setBrowse(p.id)}
                        onFocus={() => p.installed && setBrowse(p.id)}
                        onClick={() => pickProvider(p)}
                      >
                        <ProviderLogo id={p.id} size={14} />
                        <span className="grow clip">{p.name}</span>
                        {!p.installed ? <span className="pop-note">not installed</span> : value?.harness === p.id ? <Check size={13} aria-hidden /> : null}
                      </button>
                    ))}
                  </div>
                  <div className="prov-models" ref={models} role="listbox" aria-label={`${current?.name ?? "Provider"} models`} onKeyDown={(e) => rove(e)}>
                    <div className="pop-label">Model</div>
                    {current ? (
                      <>
                        <button
                          type="button"
                          role="option"
                          data-rove
                          aria-selected={value?.harness === current.id && !value.model}
                          className="pop-item"
                          onClick={() => {
                            onChange({ harness: current.id, model: null });
                            setOpen(false);
                          }}
                        >
                          <span className="grow clip">
                            Provider default
                            {current.models.find((m) => m.default) && <span className="pop-note"> · {current.models.find((m) => m.default)!.name}</span>}
                          </span>
                          {value?.harness === current.id && !value.model && <Check size={13} aria-hidden />}
                        </button>
                        {current.models.map((m) => (
                          <button
                            key={m.id}
                            type="button"
                            role="option"
                            data-rove
                            aria-selected={value?.harness === current.id && value.model === m.id}
                            className="pop-item"
                            onClick={() => {
                              onChange({ harness: current.id, model: m.id });
                              setOpen(false);
                            }}
                          >
                            <span className="grow clip">{m.name}</span>
                            {value?.harness === current.id && value.model === m.id && <Check size={13} aria-hidden />}
                          </button>
                        ))}
                        {current.error && <div className="pop-note pad-x">{current.error}</div>}
                      </>
                    ) : (
                      <div className="pop-note pad-x">Pick a provider first.</div>
                    )}
                  </div>
                </div>
              </motion.div>
            </Popover.Content>
          </Popover.Portal>
      )}
    </Popover.Root>
  );
}
