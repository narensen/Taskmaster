// The top of the window: the path to where you are on the left, actions and the window's
// own controls on the right, on the same surface as the app below it. The bar is the
// window's drag area in the desktop app.

import { useEffect, useState, type ReactNode } from "react";
import { Minus, Moon, Plus, Search, Square, Copy, Sun, SquareTerminal, X, PanelLeft } from "lucide-react";
import { Link } from "../router";
import { MOD, isDesktop, isMac, ownTitleBar, windowControls } from "../desktop";
import { Tip } from "../components/Tip";
import { setPrefs, usePrefs } from "../prefs";
import { GitChip } from "./GitChip";

export interface Crumb {
  label: string;
  to?: string;
}

function Controls() {
  const [max, setMax] = useState(false);
  useEffect(() => {
    void windowControls.isMaximized().then(setMax);
    let stop: (() => void) | null = null;
    void windowControls.onResized(() => void windowControls.isMaximized().then(setMax)).then((s) => (stop = s));
    return () => stop?.();
  }, []);
  return (
    <div className="win-controls" role="group" aria-label="Window">
      <button type="button" className="win-btn" aria-label="Minimize" onClick={windowControls.minimize}>
        <Minus size={14} strokeWidth={1.5} />
      </button>
      <button type="button" className="win-btn" aria-label={max ? "Restore" : "Maximize"} onClick={windowControls.toggleMaximize}>
        {max ? <Copy size={12} strokeWidth={1.5} style={{ transform: "scaleX(-1)" }} /> : <Square size={12} strokeWidth={1.5} />}
      </button>
      <button type="button" className="win-btn close" aria-label="Close" onClick={windowControls.close}>
        <X size={15} strokeWidth={1.5} />
      </button>
    </div>
  );
}

export function TitleBar({
  crumbs,
  onPalette,
  onTerminal,
  onMenu,
  terminalOpen,
  extra,
}: {
  crumbs: Crumb[];
  onPalette: () => void;
  onTerminal: () => void;
  onMenu: () => void;
  terminalOpen: boolean;
  extra?: ReactNode;
}) {
  const p = usePrefs();
  const drag = isDesktop() ? { "data-tauri-drag-region": true } : {};
  return (
    <header className={`titlebar ${isDesktop() && isMac ? "mac-lights" : ""}`} {...drag}>
      <button type="button" className="tb-btn tb-menu" aria-label="Menu" onClick={onMenu}>
        <PanelLeft size={15} strokeWidth={1.75} />
      </button>
      <nav className="crumbs" aria-label="Where you are" {...drag}>
        <ol>
          {crumbs.map((c, i) => {
            const last = i === crumbs.length - 1;
            return (
              <li key={`${i}:${c.label}`} className={last ? "here" : "parent"}>
                {i > 0 && (
                  <span className="crumb-sep" aria-hidden>
                    /
                  </span>
                )}
                {c.to && !last ? (
                  <Link to={c.to} className="crumb clip">
                    {c.label}
                  </Link>
                ) : (
                  <span className="crumb clip" aria-current={last ? "page" : undefined}>
                    {c.label}
                  </span>
                )}
              </li>
            );
          })}
        </ol>
      </nav>
      <GitChip />
      <div className="tb-fill" {...drag} />
      <div className="tb-actions">
        <Tip label="Search and commands" keys={`${MOD} K`}>
          <button type="button" className="tb-btn" aria-label="Search and commands" onClick={onPalette}>
            <Search size={14} strokeWidth={1.75} />
          </button>
        </Tip>
        {extra}
        <Tip label="Terminal" keys={`${MOD} J`}>
          <button type="button" className={`tb-btn ${terminalOpen ? "on" : ""}`} aria-label="Terminal" aria-pressed={terminalOpen} onClick={onTerminal}>
            <SquareTerminal size={14} strokeWidth={1.75} />
          </button>
        </Tip>
        <Tip label={p.theme === "dark" ? "Light theme" : "Dark theme"}>
          <button type="button" className="tb-btn" aria-label="Toggle theme" onClick={() => setPrefs({ theme: p.theme === "dark" ? "light" : "dark" })}>
            {p.theme === "dark" ? <Sun size={14} strokeWidth={1.75} /> : <Moon size={14} strokeWidth={1.75} />}
          </button>
        </Tip>
        <Tip label="New run" keys="N">
          <Link to="/runs/new" className="tb-btn tb-new" aria-label="New run">
            <Plus size={14} strokeWidth={1.75} />
          </Link>
        </Tip>
      </div>
      {ownTitleBar() && <Controls />}
    </header>
  );
}
