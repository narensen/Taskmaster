// A small router on the History API. The daemon serves index.html for any path that is
// not a file, so every route here can be opened directly or reloaded.

//
// Opening a run from its card (and going back to the gallery) runs inside a view
// transition where the browser has them: the card's frame and the run's header share a
// `view-transition-name`, so one grows into the other. The new page renders synchronously
// inside the transition, so nothing flashes empty. Other route changes skip it, because a
// view transition snapshots the whole window, and take a short opacity fade instead.

import { createContext, useContext, useEffect, useState, type AnchorHTMLAttributes, type ReactNode } from "react";
import { flushSync } from "react-dom";
import { motionOn } from "./prefs";

const PathContext = createContext("/");

type Transitioning = Document & { startViewTransition?: (update: () => void) => { finished: Promise<void> } };

let morphing = false;
/** Whether the route change being rendered is a card-to-run morph (so it needs no fade). */
export function isMorph(): boolean {
  return morphing;
}

const isRun = (p: string) => /^\/runs\/(?!new$)[^/]+$/.test(p);
const isHome = (p: string) => p === "/" || p === "/runs";

function transition(update: () => void, morph: boolean) {
  const doc = document as Transitioning;
  morphing = false;
  if (!morph || !doc.startViewTransition || !motionOn() || document.visibilityState !== "visible") {
    update();
    return;
  }
  morphing = true;
  const t = doc.startViewTransition(() => flushSync(update)) as { finished: Promise<void>; ready?: Promise<void>; updateCallbackDone?: Promise<void> };
  // A transition cut short by the next one is fine: the newer page wins.
  t.ready?.catch(() => undefined);
  t.finished.catch(() => undefined);
  t.updateCallbackDone?.catch(() => undefined);
}

export function navigate(to: string, replace = false, morph = false) {
  if (to === window.location.pathname + window.location.search) return;
  transition(() => {
    if (replace) window.history.replaceState(null, "", to);
    else window.history.pushState(null, "", to);
    window.dispatchEvent(new Event("tm-nav-start"));
    window.dispatchEvent(new PopStateEvent("popstate"));
    window.dispatchEvent(new Event("tm-nav-end"));
  }, morph);
  window.scrollTo(0, 0);
}

export function Router({ children }: { children: ReactNode }) {
  const [path, setPath] = useState(window.location.pathname);
  useEffect(() => {
    let ours = false;
    let shown = window.location.pathname;
    const update = () => {
      const next = window.location.pathname;
      // Back and forward come from the browser: between the gallery and a run they morph.
      const morph = (isHome(shown) && isRun(next)) || (isRun(shown) && isHome(next));
      shown = next;
      if (ours) setPath(next);
      else transition(() => setPath(next), morph);
    };
    const mark = () => (ours = true);
    const unmark = () => (ours = false);
    window.addEventListener("popstate", update);
    window.addEventListener("tm-nav-start", mark);
    window.addEventListener("tm-nav-end", unmark);
    return () => {
      window.removeEventListener("popstate", update);
      window.removeEventListener("tm-nav-start", mark);
      window.removeEventListener("tm-nav-end", unmark);
    };
  }, []);
  return <PathContext.Provider value={path}>{children}</PathContext.Provider>;
}

export function usePath(): string {
  return useContext(PathContext);
}

/** Match `pattern` such as `/runs/:id` against the current path. */
export function match(path: string, pattern: string): Record<string, string> | null {
  const want = pattern.split("/").filter(Boolean);
  const have = path.split("/").filter(Boolean);
  if (want.length !== have.length) return null;
  const params: Record<string, string> = {};
  for (let i = 0; i < want.length; i++) {
    const part = want[i]!;
    const value = have[i]!;
    if (part.startsWith(":")) params[part.slice(1)] = decodeURIComponent(value);
    else if (part !== value) return null;
  }
  return params;
}

export function Link({ to, children, morph = false, ...rest }: { to: string; morph?: boolean } & AnchorHTMLAttributes<HTMLAnchorElement>) {
  return (
    <a
      href={to}
      {...rest}
      onClick={(event) => {
        rest.onClick?.(event);
        if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey) return;
        event.preventDefault();
        navigate(to, false, morph);
      }}
    >
      {children}
    </a>
  );
}
