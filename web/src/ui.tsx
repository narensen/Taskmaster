import { useCallback, useEffect, useRef, useState, type ReactNode, type SetStateAction } from "react";
import { CircleAlert } from "lucide-react";
import type { Tone } from "./format";

export function Badge({ tone, children }: { tone: Tone; children: ReactNode }) {
  return <span className={`badge ${tone}`}>{children}</span>;
}

export function Dot({ tone }: { tone: Tone }) {
  return <span className={`dot ${tone}`} aria-hidden />;
}

export function Spinner({ label }: { label?: string }) {
  return (
    <div className="spinner" role="status">
      <span className="ring" aria-hidden />
      {label && <span>{label}</span>}
    </div>
  );
}

export function Empty({ title, children, icon, action }: { title: string; children?: ReactNode; icon?: ReactNode; action?: ReactNode }) {
  return (
    <div className="empty" role="status">
      {icon && (
        <span className="empty-ico" aria-hidden>
          {icon}
        </span>
      )}
      <strong>{title}</strong>
      {children && <p>{children}</p>}
      {action && <div className="empty-actions">{action}</div>}
    </div>
  );
}

/** Placeholder rows while a list loads, so the layout does not jump when it arrives. */
export function Loading({ rows = 3, tall = false, label = "Loading" }: { rows?: number; tall?: boolean; label?: string }) {
  return (
    <div className="loading-rows" role="status" aria-label={label}>
      {Array.from({ length: rows }, (_, i) => (
        <span key={i} className={`skeleton ${tall ? "tall" : ""}`} style={{ width: tall ? "100%" : `${88 - i * 14}%` }} />
      ))}
    </div>
  );
}

export function Problem({ error, onRetry }: { error: unknown; onRetry?: () => void }) {
  if (!error) return null;
  return (
    <div className="problem" role="alert">
      <CircleAlert size={15} className="problem-ico" aria-hidden />
      <span className="problem-text">{error instanceof Error ? error.message : String(error)}</span>
      {onRetry && (
        <button className="ghost small" onClick={onRetry}>
          Try again
        </button>
      )}
    </div>
  );
}

export function Stat({ label, value, note }: { label: string; value: ReactNode; note?: ReactNode }) {
  return (
    <div className="stat">
      <span className="label">{label}</span>
      <span className="value">{value}</span>
      {note && <span className="note">{note}</span>}
    </div>
  );
}

export function Tabs<T extends string>({
  tabs,
  value,
  onChange,
}: {
  tabs: { id: T; label: string; count?: number }[];
  value: T;
  onChange: (id: T) => void;
}) {
  return (
    <div className="tabs" role="tablist">
      {tabs.map((tab) => (
        <button
          key={tab.id}
          role="tab"
          aria-selected={tab.id === value}
          className={tab.id === value ? "on" : ""}
          onClick={() => onChange(tab.id)}
        >
          {tab.label}
          {tab.count !== undefined && <span className="count">{tab.count}</span>}
        </button>
      ))}
    </div>
  );
}

export function Field({ label, hint, children }: { label: string; hint?: ReactNode; children: ReactNode }) {
  return (
    <label className="field">
      <span className="label">{label}</span>
      {children}
      {hint && <span className="hint">{hint}</span>}
    </label>
  );
}

export function Code({ children }: { children: string }) {
  return <pre className="code">{children}</pre>;
}

/** Load something, and load it again on `deps` or every `every` milliseconds. */
export function useLoad<T>(load: () => Promise<T>, deps: unknown[], every?: number) {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [loading, setLoading] = useState(true);
  const latest = useRef(load);
  latest.current = load;
  // A poll that brings back the same data keeps the old value, so nothing re-renders for it.
  const seen = useRef<string | null>(null);

  const reload = useCallback(async () => {
    try {
      const value = await latest.current();
      let text: string | null = null;
      try {
        text = JSON.stringify(value) ?? null;
      } catch {
        text = null;
      }
      if (text === null || text !== seen.current) {
        seen.current = text;
        setData(value);
      }
      setError(null);
    } catch (err) {
      setError(err);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    setLoading(true);
    void reload();
    if (!every) return;
    const timer = window.setInterval(() => {
      if (document.visibilityState === "visible") void reload();
    }, every);
    return () => window.clearInterval(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);

  // A value set by hand is not what the server said, so the next load always lands.
  const set = useCallback((value: SetStateAction<T | null>) => {
    seen.current = null;
    setData(value);
  }, []);

  return { data, error, loading, reload, setData: set };
}

/** Run an action once at a time, keeping its error. */
export function useAction() {
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<unknown>(null);
  const run = useCallback(async (name: string, action: () => Promise<unknown>) => {
    setBusy(name);
    setError(null);
    try {
      await action();
      return true;
    } catch (err) {
      setError(err);
      return false;
    } finally {
      setBusy(null);
    }
  }, []);
  return { busy, error, run, clear: () => setError(null) };
}

/** An on/off switch: a button with role=switch, so Space and Enter toggle it. */
export function Switch({ on, label, onChange, disabled }: { on: boolean; label: string; onChange: (on: boolean) => void; disabled?: boolean }) {
  return (
    <button type="button" role="switch" aria-checked={on} aria-label={label} className={`switch ${on ? "on" : ""}`} disabled={disabled} onClick={() => onChange(!on)}>
      <span className="switch-knob" aria-hidden />
    </button>
  );
}
