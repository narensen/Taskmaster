import { memo, useCallback, useEffect, useMemo, useState, type FormEvent } from "react";
import { Download, FolderInput, Puzzle, RefreshCw, Search, SearchX, Trash2 } from "lucide-react";
import { api, type Skill, type SkillHit } from "../api";
import { forgetSkills } from "../composer";
import { toast } from "../overlay";
import { Empty, Loading, Problem, Switch, useLoad } from "../ui";

const HITS_PAGE_SIZE = 24;

function why(err: unknown) {
  return err instanceof Error ? err.message : String(err);
}

function installs(n: number) {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M installs`;
  if (n >= 1000) return `${Math.round(n / 1000)}k installs`;
  return `${n} install${n === 1 ? "" : "s"}`;
}

const InstalledRow = memo(function InstalledRow({ skill, update, onChanged }: { skill: Skill; update: boolean; onChanged: () => void }) {
  const [busy, setBusy] = useState<string | null>(null);
  const act = async (what: string, run: () => Promise<unknown>, done: string) => {
    setBusy(what);
    try {
      await run();
      forgetSkills();
      toast(done);
      onChanged();
    } catch (err) {
      toast(`Could not ${what} ${skill.name}`, why(err));
    } finally {
      setBusy(null);
    }
  };
  return (
    <li className={`skill-row ${skill.enabled ? "" : "off"}`} data-testid="skill">
      <Switch
        on={skill.enabled}
        label={`Offer ${skill.name} to the planner`}
        disabled={busy !== null}
        onChange={(on) => void act(on ? "turn on" : "turn off", () => api.enableSkill(skill.name, on), on ? `${skill.name} is on` : `${skill.name} is off`)}
      />
      <div className="skill-main">
        <div className="skill-title">
          <span className="strong mono">{skill.name}</span>
          {skill.source ? (
            <a className="muted small" href={`https://github.com/${skill.source.repo}/tree/HEAD/${skill.source.path}`} target="_blank" rel="noreferrer noopener">
              {skill.source.repo}
            </a>
          ) : (
            <span className="muted small">copied from this machine</span>
          )}
          {update && <span className="small ask-text">Update available</span>}
        </div>
        <p className="muted small skill-about">{skill.description || "No description in its SKILL.md."}</p>
      </div>
      <div className="skill-actions">
        {update && (
          <button className="small" disabled={busy !== null} onClick={() => void act("update", () => api.updateSkill(skill.name), `${skill.name} updated`)}>
            {busy === "update" ? <span className="spin" aria-hidden /> : <RefreshCw size={13} aria-hidden />} Update
          </button>
        )}
        <button
          className="icon"
          aria-label={`Remove ${skill.name}`}
          title="Remove from the store"
          disabled={busy !== null}
          onClick={() => void act("remove", () => api.removeSkill(skill.name), `${skill.name} removed`)}
        >
          <Trash2 size={14} aria-hidden />
        </button>
      </div>
    </li>
  );
});

const HitRow = memo(function HitRow({ hit, onInstalled }: { hit: SkillHit; onInstalled: () => void }) {
  const [busy, setBusy] = useState(false);
  const [done, setDone] = useState(hit.installed);
  useEffect(() => {
    setDone(hit.installed);
  }, [hit.installed]);
  const install = async () => {
    setBusy(true);
    try {
      await api.installSkill(hit.repo, hit.skill);
      forgetSkills();
      setDone(true);
      toast(`${hit.name} installed`, "It is on: the planner can attach it to pages.");
      onInstalled();
    } catch (err) {
      toast(`Could not install ${hit.name}`, why(err));
    } finally {
      setBusy(false);
    }
  };
  return (
    <li className="skill-row" data-testid="skill-hit">
      <div className="skill-main">
        <div className="skill-title">
          <span className="strong mono">{hit.name}</span>
          <a className="muted small" href={`https://github.com/${hit.repo}`} target="_blank" rel="noreferrer noopener">
            {hit.repo}
          </a>
          <span className="muted small">{installs(hit.installs)}</span>
        </div>
      </div>
      <div className="skill-actions">
        {done ? (
          <span className="small good-text">Installed</span>
        ) : (
          <button className="small" disabled={busy} onClick={() => void install()}>
            {busy ? <span className="spin" aria-hidden /> : <Download size={13} aria-hidden />} Install
          </button>
        )}
      </div>
    </li>
  );
});

const SkillsSearchForm = memo(function SkillsSearchForm({ initial, onSearch }: { initial: string; onSearch: (q: string) => void }) {
  const [query, setQuery] = useState(initial);
  const submit = (e: FormEvent) => {
    e.preventDefault();
    onSearch(query.trim());
  };
  return (
    <form className="search-field skills-search" onSubmit={submit} role="search">
      <Search size={15} aria-hidden />
      <input
        className="search"
        type="search"
        value={query}
        onChange={(e) => {
          const val = e.target.value;
          setQuery(val);
          if (!val.trim()) onSearch("");
        }}
        placeholder="Search published skills: react, testing, accessibility"
        aria-label="Search published skills"
        autoFocus
      />
      <button type="submit" className="small" disabled={query.trim().length < 2}>
        Search
      </button>
    </form>
  );
});

export function Skills() {
  const [searched, setSearched] = useState(() => (new URLSearchParams(window.location.search).get("q") ?? "").trim());
  const [visibleHits, setVisibleHits] = useState(HITS_PAGE_SIZE);
  const list = useLoad(() => api.skills(), []);
  const updates = useLoad(() => api.skillUpdates().catch(() => ({}) as Record<string, string>), []);
  const hits = useLoad(() => (searched.length >= 2 ? api.searchSkills(searched) : Promise.resolve(null)), [searched]);

  useEffect(() => {
    setVisibleHits(HITS_PAGE_SIZE);
    const url = searched ? `/skills?q=${encodeURIComponent(searched)}` : "/skills";
    window.history.replaceState(null, "", url);
  }, [searched]);

  const handleSearch = useCallback((q: string) => setSearched(q), []);
  const reloadList = list.reload;
  const reloadUpdates = updates.reload;
  const changed = useCallback(() => {
    void reloadList();
    void reloadUpdates();
  }, [reloadList, reloadUpdates]);

  const stored = useMemo(() => (list.data ?? []).filter((s) => s.place === "store"), [list.data]);
  const found = useMemo(() => (list.data ?? []).filter((s) => s.place !== "store"), [list.data]);
  const pending = useMemo(() => Object.keys(updates.data ?? {}).length, [updates.data]);
  const shownHits = useMemo(() => (hits.data ?? []).slice(0, visibleHits), [hits.data, visibleHits]);
  const totalHits = hits.data?.length ?? 0;

  return (
    <div className="page">
      <header className="page-head">
        <div className="grow">
          <h1>Skills</h1>
          <p className="page-lede">
            One store for every harness. The planner attaches a skill to the pages it fits, and those pages' agents read its SKILL.md
            before they work. Type <kbd>$</kbd> in the composer to name one yourself.
          </p>
        </div>
        <button onClick={() => void updates.reload()} disabled={updates.loading}>
          <RefreshCw size={14} aria-hidden /> Check for updates
        </button>
      </header>

      <SkillsSearchForm initial={searched} onSearch={handleSearch} />

      {searched.length >= 2 && (
        <section aria-labelledby="hits-h" className="skills-section">
          <h2 id="hits-h">
            Published {totalHits > 0 && <span className="muted">{totalHits}</span>}
          </h2>
          <Problem error={hits.error} onRetry={hits.reload} />
          {hits.loading && <Loading rows={3} label="Searching" />}
          {!hits.loading && hits.data && hits.data.length === 0 && <Empty title={`Nothing published matches “${searched}”`} icon={<SearchX size={18} strokeWidth={1.5} />} />}
          {!hits.loading && shownHits.length > 0 && (
            <>
              <ul className="skill-list card">
                {shownHits.map((h) => (
                  <HitRow key={h.id} hit={h} onInstalled={changed} />
                ))}
              </ul>
              {totalHits > visibleHits && (
                <div className="row center" style={{ marginTop: 10 }}>
                  <button type="button" className="ghost small" onClick={() => setVisibleHits((n) => n + HITS_PAGE_SIZE)}>
                    Show more ({totalHits - visibleHits} remaining)
                  </button>
                </div>
              )}
            </>
          )}
          <p className="muted small">Results from skills.sh. Installing reads the skill's folder from its GitHub repository.</p>
        </section>
      )}

      <section aria-labelledby="store-h" className="skills-section">
        <h2 id="store-h">
          In the store {stored.length > 0 && <span className="muted">{stored.length}</span>}
          {pending > 0 && <span className="small ask-text"> · {pending} with updates</span>}
        </h2>
        <Problem error={list.error} onRetry={list.reload} />
        {list.loading && !list.data && <Loading rows={3} tall label="Reading the store" />}
        {list.data && stored.length === 0 && (
          <Empty title="No skills yet" icon={<Puzzle size={18} strokeWidth={1.5} />}>
            Search above to install one. Every harness gets the same copy.
          </Empty>
        )}
        {stored.length > 0 && (
          <ul className="skill-list card">
            {stored.map((s) => (
              <InstalledRow key={s.name} skill={s} update={!!updates.data?.[s.name]} onChanged={changed} />
            ))}
          </ul>
        )}
      </section>

      {found.length > 0 && (
        <section aria-labelledby="found-h" className="skills-section">
          <h2 id="found-h">On this machine</h2>
          <p className="muted small">Skills in this project or your home folder that only one CLI reads. Copy one in to give it to every harness.</p>
          <ul className="skill-list card">
            {found.map((s) => (
              <li key={`${s.place}-${s.name}`} className="skill-row" data-testid="skill-found">
                <div className="skill-main">
                  <div className="skill-title">
                    <span className="strong mono">{s.name}</span>
                    <span className="muted small mono clip" title={s.path}>
                      {s.path}
                    </span>
                  </div>
                  <p className="muted small skill-about">{s.description || "No description in its SKILL.md."}</p>
                </div>
                <div className="skill-actions">
                  <button
                    className="small"
                    onClick={() =>
                      void api.importSkill(s.name).then(
                        () => {
                          forgetSkills();
                          toast(`${s.name} copied into the store`);
                          changed();
                        },
                        (err) => toast(`Could not copy ${s.name}`, why(err)),
                      )
                    }
                  >
                    <FolderInput size={13} aria-hidden /> Copy in
                  </button>
                </div>
              </li>
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}
