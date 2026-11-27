// Choose a provider and a model for a role, right where the run starts. Only what the
// providers API returned is offered; a provider that is not installed is shown but cannot
// be picked.

import * as Popover from "@radix-ui/react-popover";
import { Check, ChevronDown, Settings2, Users } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { Provider } from "../api";
import type { RoleDefault } from "../prefs";
import { ProviderLogo, providerName } from "../providers/logos";
import { STEPS, type Pages } from "./PagesPicker";
import { rove } from "./roving";

export function modelLabel(providers: Provider[], pick: RoleDefault | null): string {
  if (!pick) return "";
  const p = providers.find((x) => x.id === pick.harness);
  if (pick.model) return p?.models.find((m) => m.id === pick.model)?.name ?? pick.model;
  const def = p?.models.find((m) => m.default);
  return def ? `${def.name}` : "default model";
}

function ProviderColumns({
  providers,
  loading,
  value,
  onSelect,
  browse,
  setBrowse,
}: {
  providers: Provider[];
  loading?: boolean;
  value: RoleDefault | null;
  onSelect: (pick: RoleDefault, close: boolean) => void;
  browse: string | null;
  setBrowse: (id: string | null) => void;
}) {
  const models = useRef<HTMLDivElement>(null);
  const current = providers.find((p) => p.id === browse);

  const pickProvider = (p: Provider) => {
    setBrowse(p.id);
    if (value?.harness !== p.id) onSelect({ harness: p.id, model: null }, false);
    requestAnimationFrame(() => models.current?.querySelector<HTMLElement>("[data-rove]")?.focus());
  };

  return (
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
              onClick={() => onSelect({ harness: current.id, model: null }, true)}
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
                onClick={() => onSelect({ harness: current.id, model: m.id }, true)}
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
  );
}

export function ProviderPicker({
  label,
  providers,
  loading,
  value,
  onChange,
  compact = false,
  variant = "default",
  prefix,
  side = "bottom",
  align = "start",
  onMoreRoles,
  testId,
}: {
  /** What the picker is for, read by a screen reader: "Master provider". */
  label: string;
  providers: Provider[];
  loading?: boolean;
  value: RoleDefault | null;
  onChange: (pick: RoleDefault) => void;
  compact?: boolean;
  variant?: "default" | "pill";
  prefix?: string;
  side?: "top" | "bottom";
  align?: "start" | "end";
  onMoreRoles?: () => void;
  testId?: string;
}) {
  const [open, setOpen] = useState(false);
  const [browse, setBrowse] = useState<string | null>(value?.harness ?? null);
  useEffect(() => {
    if (open) setBrowse(value?.harness ?? providers.find((p) => p.installed)?.id ?? null);
  }, [open, value?.harness, providers]);
  // A resize while the menu is open moves the anchor. Close it rather than let it jump.
  useEffect(() => {
    if (!open) return;
    const close = () => setOpen(false);
    window.addEventListener("resize", close);
    return () => window.removeEventListener("resize", close);
  }, [open]);
  const chosen = value ? providers.find((p) => p.id === value.harness) : undefined;
  const name = value ? (chosen?.name ?? providerName(value.harness)) : null;
  const mLabel = modelLabel(providers, value);

  return (
    <Popover.Root open={open} onOpenChange={setOpen} modal={false}>
      <Popover.Trigger asChild>
        {variant === "pill" ? (
          <button
            type="button"
            className={`model-pill prov-pill-trigger ${open ? "on" : ""} ${!value ? "unset" : ""}`}
            aria-label={`${label}: ${name ? `${name}, ${mLabel}` : "not chosen"}`}
            title={name ? `${prefix ? `${prefix}: ` : ""}${name} · ${mLabel}` : `Choose ${label.toLowerCase()}`}
            data-testid={testId}
          >
            <ProviderLogo id={value?.harness} size={13} />
            <span className="clip">
              {prefix && <span className="pill-prefix">{prefix}</span>}
              {name ? (
                <>
                  <span>{name}</span>
                  <span className="pill-model"> · {mLabel}</span>
                </>
              ) : (
                <span>Choose {prefix ? prefix.toLowerCase() : "model"}</span>
              )}
            </span>
            <ChevronDown size={11} className="prov-chev" aria-hidden />
          </button>
        ) : (
          <button
            type="button"
            className={`prov-trigger ${value ? "" : "unset"} ${compact ? "compact" : ""}`}
            aria-label={`${label}: ${name ? `${name}, ${mLabel}` : "not chosen"}`}
            data-testid={testId}
          >
            <ProviderLogo id={value?.harness} size={compact ? 13 : 14} />
            {value ? (
              <span className="prov-trigger-text">
                <span className="prov-trigger-name">{name}</span>
                {!compact && <span className="prov-trigger-model">{mLabel}</span>}
              </span>
            ) : (
              <span className="prov-trigger-text">
                <span className="prov-trigger-name">Choose</span>
              </span>
            )}
            <ChevronDown size={12} className="prov-chev" aria-hidden />
          </button>
        )}
      </Popover.Trigger>
      {open && (
        <Popover.Portal>
          <Popover.Content asChild side={side} sideOffset={6} align={align} collisionPadding={12} sticky="always" hideWhenDetached>
            <div className="pop prov-pop">
              <ProviderColumns
                providers={providers}
                loading={loading}
                value={value}
                browse={browse}
                setBrowse={setBrowse}
                onSelect={(pick, close) => {
                  onChange(pick);
                  if (close) setOpen(false);
                }}
              />
              {onMoreRoles && (
                <div className="prov-pop-foot">
                  <button
                    type="button"
                    className="pop-item prov-pop-more"
                    onClick={() => {
                      setOpen(false);
                      onMoreRoles();
                    }}
                  >
                    <Settings2 size={13} aria-hidden />
                    <span>All role assignments & options…</span>
                  </button>
                </div>
              )}
            </div>
          </Popover.Content>
        </Popover.Portal>
      )}
    </Popover.Root>
  );
}

/** Combined Worker model + parallel count popover for composer bars on New Run and Gallery. */
export function WorkersPicker({
  providers,
  loading,
  value,
  onChange,
  pages,
  onPages,
  max,
  side = "top",
  align = "start",
  onMoreRoles,
}: {
  providers: Provider[];
  loading?: boolean;
  value: RoleDefault | null;
  onChange: (pick: RoleDefault) => void;
  pages: Pages | null;
  onPages: (v: Pages) => void;
  max: number | null;
  side?: "top" | "bottom";
  align?: "start" | "end";
  onMoreRoles?: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [browse, setBrowse] = useState<string | null>(value?.harness ?? null);
  useEffect(() => {
    if (open) setBrowse(value?.harness ?? providers.find((p) => p.installed)?.id ?? null);
  }, [open, value?.harness, providers]);
  useEffect(() => {
    if (!open) return;
    const close = () => setOpen(false);
    window.addEventListener("resize", close);
    return () => window.removeEventListener("resize", close);
  }, [open]);

  const chosen = value ? providers.find((p) => p.id === value.harness) : undefined;
  const workerName = value ? (chosen?.name ?? providerName(value.harness)) : null;
  const countLabel =
    pages == null
      ? "Workers"
      : pages === "max"
        ? `Max (${max ?? "…"})`
        : `${pages} worker${pages === 1 ? "" : "s"}`;

  const stepOptions: { v: Pages; label: string; off: boolean }[] = [
    ...STEPS.map((n) => ({ v: n as Pages, label: String(n), off: max !== null && n > max })),
    { v: "max", label: "Max", off: false },
  ];

  return (
    <Popover.Root open={open} onOpenChange={setOpen} modal={false}>
      <Popover.Trigger asChild>
        <button
          type="button"
          className={`model-pill prov-pill-trigger composer-worker-pill ${open ? "on" : ""} ${!value || pages == null ? "unset" : ""}`}
          aria-label={`Workers: ${countLabel}${workerName ? `, ${workerName}` : ""}`}
          title="Worker provider, model, and parallel count"
          data-testid="workers-picker"
        >
          <Users size={13} aria-hidden />
          {value?.harness && <ProviderLogo id={value.harness} size={13} />}
          <span className="clip">
            <span>{countLabel}</span>
            {workerName && <span className="pill-model"> · {workerName}</span>}
          </span>
          <ChevronDown size={11} className="prov-chev" aria-hidden />
        </button>
      </Popover.Trigger>
      {open && (
        <Popover.Portal>
          <Popover.Content asChild side={side} sideOffset={6} align={align} collisionPadding={12} sticky="always" hideWhenDetached>
            <div className="pop prov-pop workers-pop">
              <div className="workers-pop-count">
                <span className="pop-label">Parallel workers</span>
                <div className="seg mini" role="radiogroup" aria-label="Parallel workers">
                  {stepOptions.map((o) => {
                    const on = pages === o.v;
                    return (
                      <button
                        key={String(o.v)}
                        type="button"
                        role="radio"
                        aria-checked={on}
                        disabled={o.off}
                        className={`seg-item ${on ? "on" : ""}`}
                        onClick={() => onPages(o.v)}
                      >
                        <span className="seg-text">{o.label}</span>
                      </button>
                    );
                  })}
                </div>
              </div>
              <ProviderColumns
                providers={providers}
                loading={loading}
                value={value}
                browse={browse}
                setBrowse={setBrowse}
                onSelect={(pick, close) => {
                  onChange(pick);
                  if (close) setOpen(false);
                }}
              />
              {onMoreRoles && (
                <div className="prov-pop-foot">
                  <button
                    type="button"
                    className="pop-item prov-pop-more"
                    onClick={() => {
                      setOpen(false);
                      onMoreRoles();
                    }}
                  >
                    <Settings2 size={13} aria-hidden />
                    <span>All role assignments & options…</span>
                  </button>
                </div>
              )}
            </div>
          </Popover.Content>
        </Popover.Portal>
      )}
    </Popover.Root>
  );
}

