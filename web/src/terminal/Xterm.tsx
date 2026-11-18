// One xterm.js view. `TerminalSession` drives a shell on the daemon's PTY; `Transcript`
// writes a fixed text (a worker's commands and their output) and takes no input.

import { useEffect, useRef } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { api, followTerminal } from "../api";
import { usePrefs } from "../prefs";
import { terminalTheme } from "./theme";

const FONT = 'ui-monospace, "SF Mono", "JetBrains Mono", "Cascadia Mono", Menlo, Consolas, "Liberation Mono", monospace';

function make(theme: "dark" | "light", input: boolean): Terminal {
  return new Terminal({
    fontFamily: FONT,
    fontSize: 12.5,
    lineHeight: 1.25,
    cursorBlink: input,
    cursorStyle: "bar",
    cursorWidth: 2,
    disableStdin: !input,
    allowProposedApi: false,
    scrollback: 5000,
    theme: terminalTheme(theme),
    convertEol: !input,
    smoothScrollDuration: 0,
  });
}

function useFit(term: React.MutableRefObject<Terminal | null>, fit: React.MutableRefObject<FitAddon | null>, host: React.RefObject<HTMLDivElement | null>, onSize?: (cols: number, rows: number) => void) {
  useEffect(() => {
    const el = host.current;
    if (!el) return;
    let frame = 0;
    const observer = new ResizeObserver(() => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => {
        if (!el.offsetWidth || !el.offsetHeight || !fit.current || !term.current) return;
        fit.current.fit();
        onSize?.(term.current.cols, term.current.rows);
      });
    });
    observer.observe(el);
    return () => {
      cancelAnimationFrame(frame);
      observer.disconnect();
    };
  }, [term, fit, host, onSize]);
}

/**
 * A live shell. It reconnects from the last byte it saw, so hiding the drawer or losing the
 * stream never repeats or drops output. `active` focuses it.
 */
export function TerminalSession({
  id,
  active,
  onExit,
  onAltScreen,
}: {
  id: string;
  active: boolean;
  onExit: (code: number | null) => void;
  /** A full-screen program (an editor, a pager) took the screen, or gave it back. */
  onAltScreen?: (on: boolean) => void;
}) {
  const host = useRef<HTMLDivElement>(null);
  const term = useRef<Terminal | null>(null);
  const fit = useRef<FitAddon | null>(null);
  const { theme } = usePrefs();
  const exitRef = useRef(onExit);
  exitRef.current = onExit;
  const altRef = useRef(onAltScreen);
  altRef.current = onAltScreen;

  useEffect(() => {
    const t = make(theme, true);
    const f = new FitAddon();
    t.loadAddon(f);
    t.open(host.current!);
    term.current = t;
    fit.current = f;
    try {
      f.fit();
    } catch {
      // Hidden at mount: fitted on the first resize.
    }
    // Keystrokes in one frame go as one request.
    let pending = "";
    let timer = 0;
    const flush = () => {
      timer = 0;
      const data = pending;
      pending = "";
      if (data) void api.terminalInput(id, data).catch(() => undefined);
    };
    const alt = t.buffer.onBufferChange((buffer) => altRef.current?.(buffer.type === "alternate"));
    const typed = t.onData((data) => {
      pending += data;
      if (!timer) timer = window.setTimeout(flush, 4);
    });
    let offset = 0;
    let stop: (() => void) | null = null;
    let closed = false;
    const connect = () => {
      stop = followTerminal(id, offset, {
        data: (bytes, end) => {
          offset = end;
          t.write(bytes);
        },
        exit: (code) => {
          t.write(`\r\n\x1b[2m[the shell ended${code !== null ? ` with ${code}` : ""}]\x1b[0m\r\n`);
          exitRef.current(code);
        },
        lost: () => {
          if (!closed) window.setTimeout(connect, 800);
        },
      });
    };
    connect();
    return () => {
      closed = true;
      stop?.();
      typed.dispose();
      alt.dispose();
      if (timer) window.clearTimeout(timer);
      t.dispose();
      term.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id]);

  useEffect(() => {
    if (term.current) term.current.options.theme = terminalTheme(theme);
  }, [theme]);

  useEffect(() => {
    if (active) requestAnimationFrame(() => term.current?.focus());
  }, [active]);

  const sent = useRef("");
  useFit(term, fit, host, (cols, rows) => {
    const key = `${cols}x${rows}`;
    if (key === sent.current) return;
    sent.current = key;
    void api.resizeTerminal(id, cols, rows).catch(() => undefined);
  });

  return <div ref={host} className="xterm-host" data-terminal={id} />;
}

/** Read-only: writes `text` and what is appended to it. */
export function Transcript({ text, label }: { text: string; label: string }) {
  const host = useRef<HTMLDivElement>(null);
  const term = useRef<Terminal | null>(null);
  const fit = useRef<FitAddon | null>(null);
  const written = useRef(0);
  const { theme } = usePrefs();
  useEffect(() => {
    const t = make(theme, false);
    const f = new FitAddon();
    t.loadAddon(f);
    t.open(host.current!);
    term.current = t;
    fit.current = f;
    written.current = 0;
    return () => {
      t.dispose();
      term.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  useEffect(() => {
    const t = term.current;
    if (!t) return;
    if (text.length < written.current) {
      t.reset();
      written.current = 0;
    }
    // Only what is new is written: a growing transcript never redraws.
    t.write(text.slice(written.current));
    written.current = text.length;
  }, [text]);
  useEffect(() => {
    if (term.current) term.current.options.theme = terminalTheme(theme);
  }, [theme]);
  useFit(term, fit, host);
  return <div ref={host} className="xterm-host transcript" role="log" aria-label={label} />;
}
