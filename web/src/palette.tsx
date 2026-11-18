// Command-K: projects, runs, files and commands in one list, on cmdk. A leading > shows
// only commands; / outside a text field opens it on commands and skills. "Switch a role's
// provider" opens a second page with each provider and model for that seat.

import { Command } from "cmdk";
import { ArrowLeft, CornerDownLeft, FileText, Folder, Search } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api, type Skill } from "./api";
import { commands, useRunContext } from "./commands";
import { skills } from "./composer";
import { MOD } from "./desktop";
import { phaseLabel } from "./format";
import { RoleMark } from "./marks";
import { emit, toast } from "./overlay";
import { prefs, setPrefs, usePrefs } from "./prefs";
import { ProviderLogo } from "./providers/logos";
import { keys, prefetchRun, queryClient, useProjectFiles, useProviders, useSettings } from "./query";
import { rememberChoice, savedChoice, SEATS, type Seat } from "./rolechoice";
import { navigate } from "./router";
import { ordered, useRuns } from "./runs";

export interface PaletteState {
  open: boolean;
  mode: "all" | "slash";
}

/** A loose match: every character of the query in order. Lower is better; -1 is no match. */
function score(text: string, q: string): number {
  if (!q) return 0;
  const t = text.toLowerCase();
  const at = t.indexOf(q);
  if (at >= 0) return at === 0 ? 0 : 1 + at / 100;
  let i = 0;
  let gaps = 0;
  for (const ch of q) {
    const next = t.indexOf(ch, i);
    if (next < 0) return -1;
    gaps += next - i;
    i = next + 1;
  }
  return 10 + gaps / 10;
}

function best<T>(list: T[], q: string, text: (x: T) => string, limit: number): T[] {
  if (!q) return list.slice(0, limit);
  return list
    .map((x) => ({ x, s: score(text(x), q) }))
    .filter((m) => m.s >= 0)
    .sort((a, b) => a.s - b.s)
    .slice(0, limit)
    .map((m) => m.x);
}

export function openProject(path: string, name: string) {
  void api
    .openProject(path)
    .then(() => {
      setPrefs({ lastProject: path });
      void queryClient.invalidateQueries();
      navigate("/");
      toast(`Opened ${name}`, path, 3000);
    })
    .catch((err) => toast("The project did not open", err instanceof Error ? err.message : String(err)));
}

export function Palette({ state, onClose }: { state: PaletteState; onClose: () => void }) {
  const [q, setQ] = useState(state.mode === "slash" ? "/" : "");
  const [page, setPage] = useState<"root" | "roles" | Seat>("root");
  const [skillList, setSkills] = useState<Skill[]>([]);
  const { runs } = useRuns();
  const ctx = useRunContext();
  const { showArchived, theme } = usePrefs();
  const files = useProjectFiles();
  const providers = useProviders();
  const settings = useSettings();
  const projects = useQuery({ queryKey: keys.projects, queryFn: api.projects, staleTime: 30_000 });

  useEffect(() => {
    void skills().then(setSkills);
  }, []);

  const onlyCommands = q.startsWith(">") || q.startsWith("/");
  const text = (onlyCommands ? q.slice(1) : q).trim().toLowerCase();
  const go = (fn: () => void) => () => {
    onClose();
    fn();
  };

  const groups = useMemo(() => {
    const cmd = commands(ctx).map((c) => ({ ...c, group: c.group === "Go" ? "Go to" : c.group }));
    const extra = [
      { id: "theme", label: theme === "dark" ? "Switch to the light theme" : "Switch to the dark theme", group: "Commands", hint: "", run: () => setPrefs({ theme: prefs().theme === "dark" ? "light" : "dark" }) },
      { id: "roles", label: "Switch a role's provider…", group: "Commands", hint: "", run: () => {} },
      { id: "terminal", label: "Toggle the terminal", group: "Commands", hint: `${MOD} J`, run: () => emit("terminal-toggle", null) },
      { id: "shortcuts", label: "Keyboard shortcuts", group: "Commands", hint: "?", run: () => emit("shortcuts", null) },
      { id: "files", label: "Open the files panel", group: "Commands", hint: `${MOD} ⇧ E`, run: () => emit("files-toggle", null) },
    ];
    if (ctx) {
      extra.unshift({
        id: "review",
        label: "Review this run",
        group: "Commands",
        hint: ctx.id,
        run: () =>
          void api
            .reviewRun(ctx.id)
            .then(() => toast("Review started", `One reviewer is reading ${ctx.id}'s diff`, 3500))
            .catch((err) => toast("The review did not start", err instanceof Error ? err.message : String(err))),
      });
    }
    const all = [...extra, ...cmd];
    const commandHits = best(all, text, (c) => `${c.label} ${c.group}`, 40);
    const runHits = onlyCommands ? [] : best(ordered(runs, showArchived), text, ({ run }) => run.id, text ? 8 : 6);
    const fileHits = onlyCommands || !text ? [] : best(files.data ?? [], text, (f) => f, 8);
    const projectHits = onlyCommands ? [] : best(projects.data?.projects ?? [], text, (p) => p.name, text ? 6 : 4);
    const skillHits = q.startsWith("/") ? best(skillList, text, (s) => s.name, 20) : [];
    const pageHits = onlyCommands ? [] : best(ctx?.pages ?? [], text, (p) => p, 6);
    return { commandHits, runHits, fileHits, projectHits, skillHits, pageHits };
  }, [ctx, theme, text, onlyCommands, runs, showArchived, files.data, projects.data, skillList, q]);

  const saved = savedChoice(settings.data);
  const seat = page !== "root" && page !== "roles" ? SEATS.find((s) => s.id === page)! : null;

  const switchSeat = (seatId: Seat, harness: string, model: string | null, name: string) => {
    const roles = SEATS.find((s) => s.id === seatId)!.roles;
    const next = { ...saved };
    for (const r of roles) next[r] = { harness, model };
    onClose();
    void rememberChoice(next)
      .then(() => toast(`${SEATS.find((s) => s.id === seatId)!.label} now uses ${name}`, "Saved for this project", 3000))
      .catch((err) => toast("The choice was not saved", err instanceof Error ? err.message : String(err)));
  };

  return (
    <Command.Dialog
      open={state.open}
      onOpenChange={(open) => !open && onClose()}
      label="Command palette"
      shouldFilter={false}
      loop
      overlayClassName="palette-back"
      contentClassName="palette"
      onKeyDown={(e) => {
        if (page !== "root" && (e.key === "Escape" || (e.key === "Backspace" && !q))) {
          e.preventDefault();
          setPage(page === "roles" ? "root" : "roles");
          setQ("");
        }
      }}
    >
      <div className="palette-search">
        {page === "root" ? (
          <Search size={15} aria-hidden className="palette-ico" />
        ) : (
          <button type="button" className="icon palette-back-btn" aria-label="Back" onClick={() => (setPage(page === "roles" ? "root" : "roles"), setQ(""))}>
            <ArrowLeft size={15} />
          </button>
        )}
        <Command.Input
          className="palette-input"
          value={q}
          onValueChange={setQ}
          placeholder={page === "root" ? "Projects, runs, files and commands" : seat ? `${seat.label}: choose a provider and model` : "Which role?"}
          autoFocus
        />
        <kbd>esc</kbd>
      </div>
      <Command.List className="palette-list">
        <Command.Empty className="palette-empty">Nothing matches “{q}”.</Command.Empty>
        {page === "root" && (
          <>
            {groups.projectHits.length > 0 && (
              <Command.Group heading="Projects">
                {groups.projectHits.map((p) => (
                  <Command.Item key={p.path} value={`project:${p.path}`} className="pal-item" onSelect={go(() => (p.current ? navigate("/") : openProject(p.path, p.name)))}>
                    <Folder size={14} className="pal-ico" aria-hidden />
                    <span className="clip">{p.name}</span>
                    <span className="pal-meta">{p.current ? "open" : ""}</span>
                  </Command.Item>
                ))}
              </Command.Group>
            )}
            {groups.runHits.length > 0 && (
              <Command.Group heading="Runs">
                {groups.runHits.map(({ run }, i) => (
                  <Command.Item
                    key={run.id}
                    value={`run:${run.id}`}
                    className="pal-item"
                    onMouseEnter={() => prefetchRun(run.id)}
                    onSelect={go(() => navigate(`/runs/${encodeURIComponent(run.id)}`))}
                  >
                    <RoleMark role="master" size={13} className="pal-ico" />
                    <span className="clip">{run.id}</span>
                    <span className="pal-meta">
                      {phaseLabel(run)}
                      {!text && i < 9 ? ` · ${MOD} ${i + 1}` : ""}
                    </span>
                  </Command.Item>
                ))}
              </Command.Group>
            )}
            {groups.pageHits.length > 0 && (
              <Command.Group heading="Pages of this run">
                {groups.pageHits.map((p) => (
                  <Command.Item key={p} value={`page:${p}`} className="pal-item" onSelect={go(() => ctx?.focusAgent(`page:${p}`))}>
                    <RoleMark role="worker" size={13} className="pal-ico" />
                    <span className="clip">{p}</span>
                  </Command.Item>
                ))}
              </Command.Group>
            )}
            {groups.fileHits.length > 0 && (
              <Command.Group heading="Files">
                {groups.fileHits.map((f) => (
                  <Command.Item key={f} value={`file:${f}`} className="pal-item" onSelect={go(() => (ctx ? emit("preview", { path: f }) : navigate(`/files?path=${encodeURIComponent(f)}`)))}>
                    <FileText size={14} className="pal-ico" aria-hidden />
                    <span className="clip">{f.split("/").pop()}</span>
                    <span className="pal-meta clip">{f.includes("/") ? f.slice(0, f.lastIndexOf("/")) : ""}</span>
                  </Command.Item>
                ))}
              </Command.Group>
            )}
            {groups.skillHits.length > 0 && (
              <Command.Group heading="Skills">
                {groups.skillHits.map((s) => (
                  <Command.Item
                    key={s.name}
                    value={`skill:${s.name}`}
                    className="pal-item"
                    onSelect={go(() => emit("chip", { kind: "skill", label: `/${s.name}`, value: s.name, to: "master" }))}
                  >
                    <span className="clip">/{s.name}</span>
                    <span className="pal-meta">for the next note</span>
                  </Command.Item>
                ))}
              </Command.Group>
            )}
            {groups.commandHits.length > 0 && (
              <Command.Group heading="Commands">
                {groups.commandHits.map((c) => (
                  <Command.Item
                    key={c.id}
                    value={`cmd:${c.id}`}
                    className="pal-item"
                    onSelect={() => {
                      if (c.id === "roles") {
                        setPage("roles");
                        setQ("");
                        return;
                      }
                      onClose();
                      c.run();
                    }}
                  >
                    <span className="clip">{c.label}</span>
                    <span className="pal-meta">{c.hint || c.group}</span>
                  </Command.Item>
                ))}
              </Command.Group>
            )}
          </>
        )}
        {page === "roles" && (
          <Command.Group heading="Roles">
            {best(SEATS, text, (s) => s.label, 4).map((s) => {
              const pick = saved[s.roles[0]!];
              const p = providers.data?.find((x) => x.id === pick?.harness);
              return (
                <Command.Item key={s.id} value={`seat:${s.id}`} className="pal-item" onSelect={() => (setPage(s.id), setQ(""))}>
                  <RoleMark role={s.id === "checker" ? "verifier" : s.id} size={13} className="pal-ico" />
                  <span className="clip">{s.label}</span>
                  <span className="pal-meta">
                    {pick ? (
                      <>
                        <ProviderLogo id={pick.harness} size={12} /> {p?.name ?? pick.harness}
                      </>
                    ) : (
                      "not chosen"
                    )}
                  </span>
                </Command.Item>
              );
            })}
          </Command.Group>
        )}
        {seat && (
          <>
            {providers.isLoading && (
              <div className="palette-empty">
                <span className="skeleton" style={{ width: "60%" }} />
              </div>
            )}
            {(providers.data ?? [])
              .filter((p) => p.installed)
              .map((p) => {
                const options = [{ id: null as string | null, name: `${p.name} · provider default` }, ...p.models.map((m) => ({ id: m.id as string | null, name: `${p.name} · ${m.name}` }))];
                const hits = best(options, text, (o) => o.name, 30);
                if (!hits.length) return null;
                return (
                  <Command.Group key={p.id} heading={p.name}>
                    {hits.map((o) => {
                      const current = saved[seat.roles[0]!];
                      const on = current?.harness === p.id && (current.model ?? null) === o.id;
                      return (
                        <Command.Item key={`${p.id}:${o.id ?? ""}`} value={`${p.id}:${o.id ?? ""}`} className="pal-item" onSelect={() => switchSeat(seat.id, p.id, o.id, o.name)}>
                          <ProviderLogo id={p.id} size={14} className="pal-ico" />
                          <span className="clip">{o.name}</span>
                          <span className="pal-meta">{on ? "in use" : ""}</span>
                        </Command.Item>
                      );
                    })}
                  </Command.Group>
                );
              })}
          </>
        )}
      </Command.List>
      <div className="palette-foot" aria-hidden>
        <span>
          <kbd>↑</kbd>
          <kbd>↓</kbd> move
        </span>
        <span>
          <kbd>
            <CornerDownLeft size={10} />
          </kbd>{" "}
          open
        </span>
        <span>
          <kbd>&gt;</kbd> commands only
        </span>
        {page !== "root" && (
          <span>
            <kbd>esc</kbd> back
          </span>
        )}
      </div>
    </Command.Dialog>
  );
}
