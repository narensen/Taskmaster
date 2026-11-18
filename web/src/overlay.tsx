// What floats above the page: toasts (some with Undo), the right-click menu, the bottom
// sheet, and a small
// bus that lets any part of a run hand a chip to the composer or a file to the preview.

import { useEffect, useRef, useSyncExternalStore, type ReactNode } from "react";
import { AnimatePresence, motion } from "motion/react";
import * as Menu from "@radix-ui/react-dropdown-menu";
import { Toaster, toast as sonner } from "sonner";
import { bezier } from "./motion";
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

// Toasts, on Sonner. Calm and short: a line, an optional detail, and Undo where a change
// can be taken back.

export interface ToastOptions {
  action?: { label: string; run: () => void };
}

export function toast(text: string, detail?: string, ms = 6000, options: ToastOptions = {}) {
  sonner(text, {
    description: detail,
    duration: ms,
    action: options.action ? { label: options.action.label, onClick: options.action.run } : undefined,
  });
}

interface Undo {
  id: string | number;
  run: () => void;
  until: number;
}
let undos: Undo[] = [];

/**
 * A change that can be taken back for five seconds, by the button or Command-Z. This is
 * used instead of a confirm dialog: the change happens at once and Undo reverses it.
 */
export function undoable(text: string, undo: () => void, ms = 5000, detail?: string) {
  const entry: Undo = { id: 0, run: undo, until: Date.now() + ms };
  entry.id = sonner(text, {
    description: detail,
    duration: ms,
    action: {
      label: "Undo",
      onClick: () => {
        undos = undos.filter((u) => u !== entry);
        undo();
      },
    },
    onDismiss: () => (undos = undos.filter((u) => u !== entry)),
    onAutoClose: () => (undos = undos.filter((u) => u !== entry)),
  });
  undos.push(entry);
}

export function dismiss(id: string | number) {
  sonner.dismiss(id);
}

/** Run the newest Undo still on screen. True when there was one. */
export function undoLatest(): boolean {
  const live = undos.filter((u) => u.until > Date.now());
  const last = live[live.length - 1];
  if (!last) return false;
  undos = undos.filter((u) => u !== last);
  last.run();
  sonner.dismiss(last.id);
  return true;
}

export function Toasts({ theme }: { theme: "dark" | "light" }) {
  return (
    <Toaster
      theme={theme}
      position="bottom-right"
      visibleToasts={4}
      gap={8}
      offset={16}
      toastOptions={{
        unstyled: true,
        classNames: {
          toast: "toast",
          title: "toast-title",
          description: "toast-detail",
          actionButton: "toast-action",
          cancelButton: "toast-action",
          closeButton: "toast-x",
        },
      }}
    />
  );
}

// The right-click menu, on Radix: a menu opened at the pointer, with submenus, typeahead
// and arrow keys.

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
  n: number;
}

const menu = store<MenuState | null>(null);
let menuN = 0;

export function openMenu(x: number, y: number, items: MenuItem[]) {
  menu.set({ x, y, items, n: ++menuN });
}

export function closeMenu() {
  menu.set(null);
}

function MenuItems({ items }: { items: MenuItem[] }) {
  return (
    <>
      {items.map((item) =>
        item.items ? (
          <Menu.Sub key={item.label}>
            <Menu.SubTrigger className="menu-item" disabled={item.disabled}>
              <span>{item.label}</span>
              <span className="muted">›</span>
            </Menu.SubTrigger>
            <Menu.Portal>
              <Menu.SubContent className="menu" sideOffset={4} collisionPadding={8}>
                <MenuItems items={item.items} />
              </Menu.SubContent>
            </Menu.Portal>
          </Menu.Sub>
        ) : (
          <Menu.Item key={item.label} className="menu-item" disabled={item.disabled} onSelect={() => item.run?.()}>
            <span>{item.label}</span>
            {item.hint && <span className="muted small">{item.hint}</span>}
          </Menu.Item>
        ),
      )}
    </>
  );
}

export function ContextMenu() {
  const state = menu.use();
  return (
    <Menu.Root key={state?.n ?? 0} open={!!state} onOpenChange={(open) => !open && closeMenu()} modal={false}>
      <Menu.Trigger asChild>
        <span aria-hidden className="menu-anchor" style={{ left: state?.x ?? 0, top: state?.y ?? 0 }} />
      </Menu.Trigger>
      <Menu.Portal>
        <Menu.Content className="menu" align="start" sideOffset={2} collisionPadding={8} loop>
          {state && <MenuItems items={state.items} />}
        </Menu.Content>
      </Menu.Portal>
    </Menu.Root>
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
            role="dialog"
            aria-modal="true"
            aria-label="Panel"
            onClick={(e) => e.stopPropagation()}
            initial={on ? { y: 28 } : false}
            animate={{ y: 0 }}
            exit={on ? { y: 20 } : undefined}
            transition={{ duration: on ? 0.18 : 0, ease: bezier.out }}
          >
            <button type="button" className="sheet-grip" aria-label="Close panel" onClick={onClose} />
            {children}
          </motion.div>
        </motion.div>
      )}
    </AnimatePresence>
  );
}
