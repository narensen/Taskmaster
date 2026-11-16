// Command-K: runs, pages and commands. A leading > shows only commands. / outside a text
// field opens it on commands and skills.

import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import gsap from "gsap";
import { commands, useRunContext } from "./commands";
import { skills } from "./composer";
import { emit } from "./overlay";
import { navigate } from "./router";
import { ordered, useRuns } from "./runs";
import { phaseLabel } from "./format";
import { motionOn, usePrefs } from "./prefs";
import { dur, ease } from "./motion";
import type { Skill } from "./api";

export interface PaletteState {
  open: boolean;
  mode: "all" | "slash";
}

interface Item {
  id: string;
  label: string;
  group: string;
  hint?: string;
  run: () => void;
}

export function Palette({ state, onClose }: { state: PaletteState; onClose: () => void }) {
  const [q, setQ] = useState(state.mode === "slash" ? "/" : "");
  const [active, setActive] = useState(0);
  const [skillList, setSkills] = useState<Skill[]>([]);
  const input = useRef<HTMLInputElement>(null);
  const box = useRef<HTMLDivElement>(null);
  const { runs } = useRuns();
  const ctx = useRunContext();
  const { showArchived } = usePrefs();

  useEffect(() => {
    input.current?.focus();
    void skills().then(setSkills);
  }, []);

  useLayoutEffect(() => {
    if (!box.current || !motionOn()) return;
    gsap.fromTo(box.current, { opacity: 0, y: 8 }, { opacity: 1, y: 0, duration: dur.base, ease: ease.out, clearProps: "opacity,transform" });
  }, []);

  const items = useMemo<Item[]>(() => {
    const onlyCommands = q.startsWith(">") || q.startsWith("/");
    const text = (onlyCommands ? q.slice(1) : q).trim().toLowerCase();
    const list: Item[] = commands(ctx).map((c) => ({ id: c.id, label: c.label, group: c.group === "Go" ? "Command" : c.group, run: c.run }));
    if (q.startsWith("/")) {
      for (const s of skillList) {
        list.push({
          id: `skill:${s.name}`,
          label: s.name,
          group: "Skill",
          hint: "chip on the next note to the master",
          run: () => emit("chip", { kind: "skill", label: `/${s.name}`, value: s.name, to: "master" }),
        });
      }
    }
    if (!onlyCommands) {
      ordered(runs, showArchived).forEach(({ run }, i) =>
        list.unshift({
          id: `run:${run.id}`,
          label: run.id,
          group: "Run",
          hint: `${phaseLabel(run)}${i < 9 ? ` · ⌘${i + 1}` : ""}`,
          run: () => navigate(`/runs/${encodeURIComponent(run.id)}`),
        }),
      );
      for (const page of ctx?.pages ?? []) {
        list.push({ id: `page:${page}`, label: page, group: "Page", run: () => ctx?.focusAgent(`page:${page}`) });
      }
    }
    return text ? list.filter((i) => i.label.toLowerCase().includes(text) || i.group.toLowerCase().includes(text)) : list;
  }, [q, runs, ctx, skillList, showArchived]);

  useEffect(() => {
    setActive(0);
  }, [q]);

  const pick = (item: Item | undefined) => {
    if (!item) return;
    onClose();
    item.run();
  };

  return (
    <div className="palette-back" onMouseDown={onClose}>
      <div ref={box} className="palette" role="dialog" aria-label="Command palette" onMouseDown={(e) => e.stopPropagation()}>
        <input
          ref={input}
          className="palette-input"
          value={q}
          placeholder="Runs, pages and commands · > for commands only"
          onChange={(e) => setQ(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Escape") onClose();
            else if (e.key === "ArrowDown") {
              e.preventDefault();
              setActive((a) => Math.min(items.length - 1, a + 1));
            } else if (e.key === "ArrowUp") {
              e.preventDefault();
              setActive((a) => Math.max(0, a - 1));
            } else if (e.key === "Enter") {
              e.preventDefault();
              pick(items[active]);
            }
          }}
        />
        <div className="palette-list" role="listbox">
          {items.length === 0 && <div className="muted pad">Nothing matches.</div>}
          {items.map((item, i) => (
            <button
              key={item.id}
              role="option"
              aria-selected={i === active}
              className={`popup-item ${i === active ? "on" : ""}`}
              onMouseEnter={() => setActive(i)}
              onClick={() => pick(item)}
            >
              <span className="clip">{item.label}</span>
              <span className="muted small">
                {item.group}
                {item.hint ? ` · ${item.hint}` : ""}
              </span>
            </button>
          ))}
        </div>
      </div>
    </div>
  );
}
