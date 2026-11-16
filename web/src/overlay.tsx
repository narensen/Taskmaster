// What floats above the page: toasts (some with Undo), the right-click menu, and a small
// bus that lets any part of a run hand a chip to the composer or a file to the preview.

import { useEffect, useRef, useState, useSyncExternalStore, type ReactNode } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { bezier, dur } from "./motion";
import { useMotion } from "./prefs";

// A tiny store helper.
function store<T>(initial: T) {
  let value = initial;
  const listeners = new Set<() => void>();
  return {
    get: () => value,
    set: (next: T) => {
      value = next;
      listeners.forEach((l) => l());
    },
    use: () =>
      useSyncExternalStore(
        (l) => {
          listeners.add(l);
          return () => listeners.delete(l);
        },
        () => value,
      ),
  };
}

// Toasts.

export interface Toast {
  id: number;
  text: string;
  detail?: string;
  undo?: () => void;
  until: number;
}

const toasts = store<Toast[]>([]);
let nextToast = 1;

export function toast(text: string, detail?: string, ms = 6000) {
  const t: Toast = { id: nextToast++, text, detail, until: Date.now() + ms };
  toasts.set([...toasts.get(), t]);
  window.setTimeout(() => dismiss(t.id), ms);
}

/** A change that can be taken back for five seconds, by the button or Command-Z. */
export function undoable(text: string, undo: () => void) {
  const t: Toast = { id: nextToast++, text, undo, until: Date.now() + 5000 };
  toasts.set([...toasts.get().filter((x) => !x.undo), t]);
  window.setTimeout(() => dismiss(t.id), 5000);
}

export function dismiss(id: number) {
  toasts.set(toasts.get().filter((t) => t.id !== id));
}

/** Run the newest Undo still on screen. True when there was one. */
export function undoLatest(): boolean {
  const live = toasts.get().filter((t) => t.undo && t.until > Date.now());
  const last = live[live.length - 1];
  if (!last) return false;
  last.undo!();
  dismiss(last.id);
  return true;
}

export function Toasts() {
  const list = toasts.use();
  const on = useMotion();
  return (
    <div className="toasts" role="status" aria-live="polite">
      <AnimatePresence initial={false}>
        {list.map((t) => (
          <motion.div
            key={t.id}
            className="toast"
            initial={on ? { opacity: 0, y: 8 } : false}
            animate={{ opacity: 1, y: 0 }}
            exit={on ? { opacity: 0, y: 6 } : { opacity: 1, transition: { duration: 0 } }}
            transition={{ duration: on ? dur.base : 0, ease: bezier.out }}
          >
            <div className="grow">
              <div>{t.text}</div>
              {t.detail && <div className="muted small break">{t.detail}</div>}
            </div>
            {t.undo && (
              <button
                className="ghost small"
                onClick={() => {
                  t.undo!();
                  dismiss(t.id);
                }}
              >
                Undo
              </button>
            )}
            <button className="ghost small" aria-label="Dismiss" onClick={() => dismiss(t.id)}>
              ×
            </button>
          </motion.div>
        ))}
      </AnimatePresence>
    </div>
  );
}

// The right-click menu.

export interface MenuItem {
  label: string;
  run?: () => void;
  items?: MenuItem[];
  disabled?: boolean;
  hint?: string;
}

interface MenuState {
  x: number;
  y: number;
  items: MenuItem[];
}

const menu = store<MenuState | null>(null);

export function openMenu(x: number, y: number, items: MenuItem[]) {
  menu.set({ x, y, items });
}

export function closeMenu() {
  menu.set(null);
}

function MenuList({ items, onDone }: { items: MenuItem[]; onDone: () => void }) {
  const [open, setOpen] = useState<number | null>(null);
  return (
    <ul className="menu-list" role="menu">
      {items.map((item, i) => (
        <li key={item.label} className="menu-li" onMouseEnter={() => setOpen(item.items ? i : null)}>
          <button
            role="menuitem"
            className="menu-item"
            disabled={item.disabled}
            onClick={() => {
              if (item.items) {
                setOpen(open === i ? null : i);
                return;
              }
              item.run?.();
              onDone();
            }}
          >
            <span>{item.label}</span>
            {item.items ? <span className="muted">›</span> : item.hint ? <span className="muted small">{item.hint}</span> : null}
          </button>
          {item.items && open === i && (
            <div className="submenu">
              <MenuList items={item.items} onDone={onDone} />
            </div>
          )}
        </li>
      ))}
    </ul>
  );
}

export function ContextMenu() {
  const state = menu.use();
  const on = useMotion();
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!state) return;
    const close = (event: Event) => {
      if (ref.current && event.target instanceof Node && ref.current.contains(event.target)) return;
      closeMenu();
    };
    const key = (event: KeyboardEvent) => event.key === "Escape" && closeMenu();
    window.addEventListener("pointerdown", close, true);
    window.addEventListener("keydown", key);
    window.addEventListener("resize", closeMenu);
    return () => {
      window.removeEventListener("pointerdown", close, true);
      window.removeEventListener("keydown", key);
      window.removeEventListener("resize", closeMenu);
    };
  }, [state]);
  return (
    <AnimatePresence>
      {state && (
        <motion.div
          key="menu"
          ref={ref}
          className="menu"
          style={{ left: Math.max(4, Math.min(state.x, window.innerWidth - 240)), top: Math.max(4, Math.min(state.y, window.innerHeight - 40 * state.items.length - 16)) }}
          initial={on ? { opacity: 0, y: 4 } : false}
          animate={{ opacity: 1, y: 0 }}
          exit={on ? { opacity: 0, y: 4 } : { opacity: 1, transition: { duration: 0 } }}
          transition={{ duration: on ? dur.fast : 0, ease: bezier.out }}
        >
          <MenuList items={state.items} onDone={closeMenu} />
        </motion.div>
      )}
    </AnimatePresence>
  );
}

// The bus.

export interface Chip {
  kind: "file" | "page" | "agent" | "key" | "pr" | "skill" | "cite" | "search" | "attachment";
  label: string;
  /** What the chip stands for when the message is sent or copied. */
  value: string;
  /** For a file: a line; for a pr or search result: the address. */
  line?: number;
  url?: string;
  /** Who a cite or skill is for. */
  to?: string;
  comment?: string;
  thumb?: string;
  bytes?: number;
}

type Handler<T> = (value: T) => void;
const handlers = new Map<string, Set<Handler<unknown>>>();

export function emit<T>(name: string, value: T) {
  handlers.get(name)?.forEach((h) => h(value));
}

export function useOn<T>(name: string, handler: Handler<T>) {
  const latest = useRef(handler);
  latest.current = handler;
  useEffect(() => {
    const h: Handler<unknown> = (v) => latest.current(v as T);
    if (!handlers.has(name)) handlers.set(name, new Set());
    handlers.get(name)!.add(h);
    return () => {
      handlers.get(name)!.delete(h);
    };
  }, [name]);
}

export function Sheet({ open, onClose, children }: { open: boolean; onClose: () => void; children: ReactNode }) {
  const on = useMotion();
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);
  return (
    <AnimatePresence>
      {open && (
        <motion.div
          className="sheet-back"
          onClick={onClose}
          initial={on ? { opacity: 0 } : false}
          animate={{ opacity: 1 }}
          exit={on ? { opacity: 0 } : undefined}
          transition={{ duration: on ? 0.16 : 0, ease: bezier.out }}
        >
          <motion.div
            className="sheet"
            onClick={(e) => e.stopPropagation()}
            initial={on ? { y: 28 } : false}
            animate={{ y: 0 }}
            exit={on ? { y: 20 } : undefined}
            transition={{ duration: on ? 0.18 : 0, ease: bezier.out }}
          >
            <div className="sheet-grip" onClick={onClose} />
            {children}
          </motion.div>
        </motion.div>
      )}
    </AnimatePresence>
  );
}
