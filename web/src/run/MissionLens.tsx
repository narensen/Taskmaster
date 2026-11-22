// Lens 1 — Spatial Mission Control:
// 1. Live Execution DAG (Contracts -> layered parallel dependency Units -> Verification gate)
//    with rich semantic visual coding, SVG bezier dependency edges, live diff stats, and status pulses.
// 2. Node Focus Drawer with Targeted Soft-Steer (notes straight to `page:<id>`), Restart, Diff, and Stream jump.
// 3. Interactive Plan Workbench during `awaiting_approval` so humans can edit briefs, globs,
//    difficulty tiers, dependencies, and contracts inline before approving!

import { useEffect, useMemo, useState } from "react";
import {
  AlertTriangle,
  ArrowRight,
  Check,
  CheckCircle2,
  Circle,
  Edit3,
  FileCode2,
  GitBranch,
  Layers,
  Loader2,
  Plus,
  RotateCw,
  Save,
  Send,
  ShieldCheck,
  Sparkles,
  Square,
  Trash2,
  X,
} from "lucide-react";
import {
  api,
  type ContractFile,
  type DiffStats,
  type PagePlan,
  type PageState,
  type PageView,
  type Plan,
  type RunDetail,
} from "../api";
import { usd } from "../format";
import { toast } from "../overlay";
import { GlobFiles } from "./GlobFiles";
import { shownPageState, type Surface } from "./model";

type ShownState = PageState | "stopped" | undefined;

interface DagNode {
  page: PagePlan;
  view?: PageView;
  surface?: Surface;
  layer: number;
  row: number;
  x: number;
  y: number;
}

const NODE_W = 248;
const NODE_H = 148;
const COL_GAP = 76;
const ROW_GAP = 22;
const PAD_X = 26;
const PAD_Y = 24;

function computeLayers(pages: PagePlan[]): Map<string, number> {
  const byId = new Map(pages.map((p) => [p.id, p]));
  const memo = new Map<string, number>();
  const visiting = new Set<string>();
  const depth = (id: string): number => {
    if (memo.has(id)) return memo.get(id)!;
    if (visiting.has(id)) return 0;
    visiting.add(id);
    const p = byId.get(id);
    const deps = p?.depends_on ?? [];
    let d = 0;
    for (const dep of deps) {
      if (byId.has(dep)) d = Math.max(d, depth(dep) + 1);
    }
    visiting.delete(id);
    memo.set(id, d);
    return d;
  };
  for (const p of pages) depth(p.id);
  return memo;
}

function statusClass(state: ShownState): string {
  if (state === "stopped") return "st-stopped";
  if (state === "passed") return "st-passed";
  if (state === "building") return "st-building";
  if (state === "flagged") return "st-flagged";
  if (state === "failed") return "st-failed";
  return "st-queued";
}

function StateGlyph({ state }: { state: ShownState }) {
  if (state === "stopped") return <Square size={11} strokeWidth={2.2} className="dag-ico stopped" aria-hidden />;
  if (state === "passed") return <CheckCircle2 size={14} className="dag-ico passed" aria-hidden />;
  if (state === "building") return <Loader2 size={14} className="dag-ico building spin-ico" aria-hidden />;
  if (state === "flagged") return <AlertTriangle size={14} className="dag-ico flagged" aria-hidden />;
  if (state === "failed") return <AlertTriangle size={14} className="dag-ico failed" aria-hidden />;
  return <Circle size={13} className="dag-ico queued" aria-hidden />;
}

function clonePlan(plan: Plan): Plan {
  return JSON.parse(JSON.stringify(plan)) as Plan;
}

export function MissionLens({
  id,
  detail,
  surfaces,
  diffStats,
  onReload,
  onRestartWorker,
  onOpenDiff,
  onJumpToStream,
  onApproved,
}: {
  id: string;
  detail: RunDetail;
  surfaces: Surface[];
  diffStats: DiffStats | null;
  onReload: () => void;
  onRestartWorker: (page: string) => void;
  onOpenDiff: (page: string) => void;
  onJumpToStream: (agentId: string) => void;
  onApproved: () => void;
}) {
  const plan = detail.plan;
  const awaiting = detail.phase === "awaiting_approval" && !detail.active;
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [steerText, setSteerText] = useState("");
  const [steering, setSteering] = useState(false);

  // Interactive Plan Workbench state
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState<Plan | null>(null);
  const [savingPlan, setSavingPlan] = useState(false);
  const [approving, setApproving] = useState(false);

  useEffect(() => {
    if (!awaiting) {
      setEditing(false);
      setDraft(null);
    }
  }, [awaiting]);

  const startEdit = () => {
    if (!plan) return;
    setDraft(clonePlan(plan));
    setEditing(true);
    setSelectedId(null);
  };

  const activePlan = editing && draft ? draft : plan;

  // Build DAG geometry
  const graph = useMemo(() => {
    const pages = activePlan?.pages ?? [];
    const contracts = activePlan?.contracts ?? [];
    const hasContracts = contracts.length > 0;
    const layers = computeLayers(pages);
    const byLayer = new Map<number, PagePlan[]>();
    let maxLayer = 0;
    for (const p of pages) {
      const l = layers.get(p.id) ?? 0;
      maxLayer = Math.max(maxLayer, l);
      const bucket = byLayer.get(l) ?? [];
      bucket.push(p);
      byLayer.set(l, bucket);
    }

    const viewMap = new Map(detail.pages.map((p) => [p.id, p]));
    const surfaceMap = new Map<string, Surface>();
    for (const s of surfaces) {
      if (s.page && !surfaceMap.has(s.page)) surfaceMap.set(s.page, s);
    }

    const xOffset = hasContracts ? NODE_W + COL_GAP : 0;
    const nodes: DagNode[] = [];
    const nodeById = new Map<string, DagNode>();
    let maxRows = hasContracts ? 1 : 0;

    for (let l = 0; l <= maxLayer; l++) {
      const col = byLayer.get(l) ?? [];
      maxRows = Math.max(maxRows, col.length);
      col.forEach((p, row) => {
        const node: DagNode = {
          page: p,
          view: viewMap.get(p.id),
          surface: surfaceMap.get(p.id),
          layer: l,
          row,
          x: PAD_X + xOffset + l * (NODE_W + COL_GAP),
          y: PAD_Y + row * (NODE_H + ROW_GAP),
        };
        nodes.push(node);
        nodeById.set(p.id, node);
      });
    }

    const verifyX = PAD_X + xOffset + (pages.length ? (maxLayer + 1) * (NODE_W + COL_GAP) : 0);
    const totalH = Math.max(220, PAD_Y * 2 + Math.max(1, maxRows) * NODE_H + Math.max(0, maxRows - 1) * ROW_GAP);
    const contractY = Math.round((totalH - NODE_H) / 2);
    const verifyY = Math.round((totalH - NODE_H) / 2);
    const totalW = verifyX + 204 + PAD_X;

    // Edges: Contracts -> Layer 0, Dependencies -> Consumers, Leaf nodes -> Verify gate
    const dependents = new Set<string>();
    for (const n of nodes) {
      for (const dep of n.page.depends_on ?? []) dependents.add(dep);
    }

    const edges: { key: string; d: string; kind: "contract" | "dep" | "verify"; tone: string }[] = [];
    const bezier = (x1: number, y1: number, x2: number, y2: number) => {
      const mid = (x1 + x2) / 2;
      return `M ${x1} ${y1} C ${mid} ${y1}, ${mid} ${y2}, ${x2} ${y2}`;
    };

    if (hasContracts) {
      for (const n of nodes) {
        if (n.layer === 0) {
          edges.push({
            key: `c->${n.page.id}`,
            d: bezier(PAD_X + NODE_W, contractY + NODE_H / 2, n.x, n.y + NODE_H / 2),
            kind: "contract",
            tone: "edge-contract",
          });
        }
      }
    }

    for (const n of nodes) {
      for (const dep of n.page.depends_on ?? []) {
        const src = nodeById.get(dep);
        if (!src) continue;
        const st = shownPageState(src.view?.state, detail);
        const tone = st === "passed" ? "edge-passed" : st === "failed" ? "edge-failed" : st === "building" ? "edge-building" : "edge-queued";
        edges.push({
          key: `${dep}->${n.page.id}`,
          d: bezier(src.x + NODE_W, src.y + NODE_H / 2, n.x, n.y + NODE_H / 2),
          kind: "dep",
          tone,
        });
      }
      if (!dependents.has(n.page.id)) {
        const st = shownPageState(n.view?.state, detail);
        const tone = st === "passed" ? "edge-passed" : st === "failed" ? "edge-failed" : st === "building" ? "edge-building" : "edge-queued";
        edges.push({
          key: `${n.page.id}->verify`,
          d: bezier(n.x + NODE_W, n.y + NODE_H / 2, verifyX, verifyY + NODE_H / 2),
          kind: "verify",
          tone,
        });
      }
    }

    return {
      hasContracts,
      contracts,
      nodes,
      nodeById,
      edges,
      contractY,
      verifyX,
      verifyY,
      totalW,
      totalH,
    };
  }, [activePlan, detail, surfaces]);

  const selected = selectedId ? graph.nodeById.get(selectedId) ?? null : null;

  const sendSteer = async () => {
    if (!selected || !steerText.trim()) return;
    setSteering(true);
    try {
      await api.addNote(id, `page:${selected.page.id}`, steerText.trim());
      setSteerText("");
      toast("Steer note queued", `Sent to worker ${selected.page.id}`, 2500);
      onReload();
    } catch (err) {
      toast("Could not send steer note", err instanceof Error ? err.message : String(err));
    } finally {
      setSteering(false);
    }
  };

  const savePlanChanges = async () => {
    if (!draft) return;
    setSavingPlan(true);
    try {
      await api.updatePlan(id, draft);
      setEditing(false);
      setDraft(null);
      toast("Plan updated", "Saved edited units and dependency graph", 2500);
      onReload();
    } catch (err) {
      toast("Plan validation failed", err instanceof Error ? err.message : String(err));
    } finally {
      setSavingPlan(false);
    }
  };

  const approveAndBuild = async () => {
    setApproving(true);
    try {
      if (editing && draft) {
        await api.updatePlan(id, draft);
        setEditing(false);
        setDraft(null);
      }
      await api.approve(id);
      onApproved();
    } catch (err) {
      toast("Could not approve plan", err instanceof Error ? err.message : String(err));
    } finally {
      setApproving(false);
    }
  };

  if (!activePlan) {
    return (
      <div className="mission-empty" data-testid="mission-lens">
        <div className="mission-empty-card">
          <Sparkles size={22} className="mission-ico-accent" aria-hidden />
          <h3>Waiting for execution plan</h3>
          <p className="muted">
            {detail.phase === "planning"
              ? "The Master is inspecting the repository and drafting parallel work units and shared contracts…"
              : "This run has no structured DAG plan yet."}
          </p>
        </div>
      </div>
    );
  }

  const passedCount = detail.pages.filter((p) => p.state === "passed").length;
  const buildingCount = detail.pages.filter((p) => shownPageState(p.state, detail) === "building").length;
  const failedCount = detail.pages.filter((p) => p.state === "failed" || p.state === "flagged").length;

  return (
    <div className="mission-lens" data-testid="mission-lens">
      {/* Top KPI & Plan Summary Strip */}
      <div className="mission-header">
        <div className="mission-header-copy">
          <div className="mission-kpis">
            <span className="studio-pill tone-indigo">
              <Layers size={12} aria-hidden /> {activePlan.pages.length} unit{activePlan.pages.length === 1 ? "" : "s"}
            </span>
            {activePlan.contracts.length > 0 && (
              <span className="studio-pill tone-cyan">
                <FileCode2 size={12} aria-hidden /> {activePlan.contracts.length} contract{activePlan.contracts.length === 1 ? "" : "s"}
              </span>
            )}
            <span className="studio-pill tone-emerald">{passedCount} passed</span>
            {buildingCount > 0 && <span className="studio-pill tone-blue">{buildingCount} active</span>}
            {failedCount > 0 && <span className="studio-pill tone-rose">{failedCount} issue{failedCount === 1 ? "" : "s"}</span>}
            {detail.parallel && <span className="studio-pill tone-muted">{detail.parallel}× parallel</span>}
          </div>
          {activePlan.summary && <p className="mission-summary">{activePlan.summary}</p>}
        </div>
        <div className="mission-header-actions">
          {awaiting && !editing && (
            <button type="button" className="small" onClick={startEdit} data-testid="edit-plan-btn">
              <Edit3 size={13} aria-hidden /> Edit plan
            </button>
          )}
          {awaiting && editing && (
            <>
              <button
                type="button"
                className="ghost small"
                onClick={() => {
                  setEditing(false);
                  setDraft(null);
                }}
              >
                Cancel edits
              </button>
              <button type="button" className="small" disabled={savingPlan} onClick={() => void savePlanChanges()}>
                <Save size={13} aria-hidden /> {savingPlan ? "Saving…" : "Save plan"}
              </button>
            </>
          )}
          {awaiting && (
            <button
              type="button"
              className="primary small"
              disabled={approving}
              onClick={() => void approveAndBuild()}
              data-testid="mission-approve-btn"
            >
              <Check size={13} aria-hidden /> {approving ? "Starting…" : editing ? "Save & Approve" : "Approve & Build"}
            </button>
          )}
        </div>
      </div>

      {/* Visual Spatial DAG Canvas */}
      <div className="dag-viewport" role="region" aria-label="Execution DAG">
        <div className="dag-stage" style={{ width: graph.totalW, height: graph.totalH }}>
          <svg className="dag-edges" width={graph.totalW} height={graph.totalH} aria-hidden>
            <defs>
              <marker id="dag-arrow" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
                <path d="M 0 1.5 L 8.5 5 L 0 8.5 z" fill="currentColor" />
              </marker>
            </defs>
            {graph.edges.map((e) => (
              <path
                key={e.key}
                d={e.d}
                className={`dag-edge ${e.tone}`}
                markerEnd="url(#dag-arrow)"
              />
            ))}
          </svg>

          {/* Contracts Column */}
          {graph.hasContracts && (
            <div
              className="dag-node dag-contract-node"
              style={{ left: PAD_X, top: graph.contractY, width: NODE_W, height: NODE_H }}
            >
              <div className="dag-node-top">
                <span className="dag-node-kind tone-cyan">
                  <FileCode2 size={13} aria-hidden /> Shared Contracts
                </span>
                <span className="mono small muted">{graph.contracts.length}</span>
              </div>
              <ul className="dag-contract-list mono">
                {graph.contracts.slice(0, 3).map((c) => (
                  <li key={c.path} className="clip" title={c.path}>
                    {c.path}
                  </li>
                ))}
                {graph.contracts.length > 3 && <li className="muted">+{graph.contracts.length - 3} more</li>}
              </ul>
            </div>
          )}

          {/* Parallel Unit Nodes */}
          {graph.nodes.map((node) => {
            const p = node.page;
            const st = shownPageState(node.view?.state, detail);
            const diff = diffStats?.pages[p.id];
            const isSel = selectedId === p.id;
            const diffBadge = p.difficulty?.toLowerCase();
            return (
              <button
                key={p.id}
                type="button"
                className={`dag-node dag-unit-node ${statusClass(st)} ${isSel ? "selected" : ""}`}
                style={{ left: node.x, top: node.y, width: NODE_W, height: NODE_H }}
                onClick={() => setSelectedId(isSel ? null : p.id)}
                data-testid={`dag-node-${p.id}`}
              >
                <div className="dag-node-top">
                  <span className="dag-node-id">
                    <StateGlyph state={st} />
                    <strong className="clip">{p.id}</strong>
                  </span>
                  <span className="dag-node-badges">
                    {diffBadge && (
                      <span className={`dag-mini-pill diff-${diffBadge === "hard" ? "hard" : diffBadge === "easy" ? "easy" : "normal"}`}>
                        {diffBadge}
                      </span>
                    )}
                    {node.view?.tier && node.view.tier !== "worker" && (
                      <span className="dag-mini-pill tier-up">{node.view.tier}</span>
                    )}
                  </span>
                </div>
                <GlobFiles glob={p.owned_glob} max={4} interactive={false} edited={node.surface?.summary.editedFiles} className="dag-node-files" />
                <div className="dag-node-brief">
                  {node.surface?.summary.headline || p.brief}
                </div>
                <div className="dag-node-foot mono">
                  <span>{st ?? "queued"}</span>
                  {node.view && node.view.repairs > 0 && (
                    <span className="dag-repairs">{node.view.repairs} repair{node.view.repairs === 1 ? "" : "s"}</span>
                  )}
                  {diff && (diff.added > 0 || diff.removed > 0) && (
                    <span className="dag-diff">
                      <span className="diff-add">+{diff.added}</span>
                      {diff.removed > 0 && <span className="diff-del"> -{diff.removed}</span>}
                    </span>
                  )}
                  {node.surface && node.surface.cost > 0 && <span>{usd(node.surface.cost)}</span>}
                </div>
              </button>
            );
          })}

          {/* Verification & Merge Gate */}
          <div
            className={`dag-node dag-verify-node ${
              detail.phase === "done"
                ? "st-passed"
                : detail.phase === "verifying" || detail.phase === "reviewing"
                  ? "st-building"
                  : detail.phase === "failed"
                    ? "st-failed"
                    : "st-queued"
            }`}
            style={{ left: graph.verifyX, top: graph.verifyY, width: 204, height: NODE_H }}
          >
            <div className="dag-node-top">
              <span className="dag-node-kind tone-violet">
                <ShieldCheck size={14} aria-hidden /> Verify & Merge
              </span>
            </div>
            <div className="dag-node-brief">
              {detail.phase === "done"
                ? detail.merged
                  ? `Merged into ${detail.base || "branch"}`
                  : "All checks passed"
                : detail.phase === "verifying"
                  ? "Running deterministic project checks…"
                  : detail.phase === "reviewing"
                    ? "Cross-family adversarial review…"
                    : "Static checks, tests, and safe merge"}
            </div>
            <div className="dag-node-foot mono">
              <span>{passedCount}/{activePlan.pages.length} ready</span>
              {detail.branch && (
                <span className="clip">
                  <GitBranch size={11} aria-hidden /> {detail.branch.replace(/^taskmaster\//, "")}
                </span>
              )}
            </div>
          </div>
        </div>
      </div>

      {/* Node Focus Drawer (Inspect + Targeted Soft-Steer) */}
      {selected && !editing && (
        <section className="mission-drawer" aria-label={`Unit ${selected.page.id} details`}>
          <header className="mission-drawer-head">
            <div className="row gap">
              <StateGlyph state={shownPageState(selected.view?.state, detail)} />
              <h3>{selected.page.id}</h3>
              {selected.page.difficulty && <span className="studio-pill tone-violet">{selected.page.difficulty}</span>}
              {selected.view?.tier && <span className="studio-pill tone-muted">tier: {selected.view.tier}</span>}
            </div>
            <div className="row gap">
              <button
                type="button"
                className="ghost small"
                onClick={() => onJumpToStream(`page:${selected.page.id}`)}
              >
                <ArrowRight size={13} aria-hidden /> Stream
              </button>
              <button type="button" className="ghost small" onClick={() => onOpenDiff(selected.page.id)}>
                Diff
              </button>
              <button
                type="button"
                className="small"
                onClick={() => onRestartWorker(selected.page.id)}
                title="Restart this worker without cancelling sibling parallel workers"
              >
                <RotateCw size={13} aria-hidden /> Restart unit
              </button>
              <button type="button" className="icon" aria-label="Close inspector" onClick={() => setSelectedId(null)}>
                <X size={15} />
              </button>
            </div>
          </header>

          <div className="mission-drawer-grid">
            <div className="mission-drawer-col">
              <div className="mission-field-label">Files</div>
              <GlobFiles glob={selected.page.owned_glob} edited={selected.surface?.summary.editedFiles} className="mission-drawer-files" />
              <div className="mission-field-label">Brief</div>
              <p className="mission-drawer-text">{selected.page.brief}</p>
              {selected.page.done && (
                <>
                  <div className="mission-field-label">Done criteria</div>
                  <p className="mission-drawer-text muted">{selected.page.done}</p>
                </>
              )}
              {(selected.page.depends_on?.length ?? 0) > 0 && (
                <div className="mission-deps-row">
                  <span className="mission-field-label">Depends on:</span>
                  {selected.page.depends_on!.map((d) => (
                    <button key={d} type="button" className="studio-pill tone-indigo" onClick={() => setSelectedId(d)}>
                      {d}
                    </button>
                  ))}
                </div>
              )}
            </div>

            <div className="mission-drawer-col">
              <div className="mission-field-label">Soft-Steer worker ({selected.page.id})</div>
              <p className="muted small">
                Inject targeted guidance into this unit without stopping sibling workers.
              </p>
              <div className="mission-steer-row">
                <input
                  type="text"
                  value={steerText}
                  onChange={(e) => setSteerText(e.target.value)}
                  placeholder={`Guide ${selected.page.id} (e.g. "Keep the existing export signature")…`}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") void sendSteer();
                  }}
                />
                <button
                  type="button"
                  className="primary small"
                  disabled={steering || !steerText.trim()}
                  onClick={() => void sendSteer()}
                >
                  <Send size={13} aria-hidden /> Steer
                </button>
              </div>
              {selected.surface && selected.surface.summary.editedFiles.length > 0 && (
                <div className="mission-files-list">
                  <div className="mission-field-label">Modified files</div>
                  <div className="row gap wrap">
                    {selected.surface.summary.editedFiles.slice(0, 8).map((f) => (
                      <code key={f} className="studio-pill tone-muted mono">{f}</code>
                    ))}
                  </div>
                </div>
              )}
            </div>
          </div>
        </section>
      )}

      {/* Interactive Plan Workbench (during awaiting_approval) */}
      {awaiting && editing && draft && (
        <section className="plan-workbench" aria-label="Interactive plan workbench" data-testid="plan-workbench">
          <div className="plan-workbench-head">
            <div>
              <h3>Interactive Plan Workbench</h3>
              <p className="muted small">
                Adjust unit scope, owned globs, difficulty tier, dependencies, or contracts. The DAG above updates live.
              </p>
            </div>
            <button
              type="button"
              className="small"
              onClick={() => {
                const nextNum = draft.pages.length + 1;
                const newPage: PagePlan = {
                  id: `unit-${nextNum}`,
                  route: "",
                  owned_glob: `src/unit_${nextNum}/**`,
                  cites: [],
                  brief: "Describe what this unit should implement.",
                  done: "",
                  depends_on: [],
                  difficulty: "normal",
                };
                setDraft({ ...draft, pages: [...draft.pages, newPage] });
              }}
            >
              <Plus size={13} aria-hidden /> Add unit
            </button>
          </div>

          <div className="workbench-units">
            {draft.pages.map((p, idx) => {
              const otherIds = draft.pages.filter((_, j) => j !== idx).map((x) => x.id);
              const updatePage = (patch: Partial<PagePlan>) => {
                const next = draft.pages.map((item, j) => (j === idx ? { ...item, ...patch } : item));
                setDraft({ ...draft, pages: next });
              };
              const toggleDep = (depId: string) => {
                const cur = p.depends_on ?? [];
                const nextDeps = cur.includes(depId) ? cur.filter((x) => x !== depId) : [...cur, depId];
                updatePage({ depends_on: nextDeps });
              };
              return (
                <div key={idx} className="workbench-unit-card">
                  <div className="workbench-unit-row">
                    <label className="workbench-field">
                      <span>Unit ID</span>
                      <input
                        className="mono"
                        value={p.id}
                        onChange={(e) => updatePage({ id: e.target.value })}
                      />
                    </label>
                    <label className="workbench-field grow">
                      <span>Owned Glob</span>
                      <input
                        className="mono"
                        value={p.owned_glob}
                        onChange={(e) => updatePage({ owned_glob: e.target.value })}
                      />
                    </label>
                    <label className="workbench-field">
                      <span>Difficulty</span>
                      <select
                        value={p.difficulty ?? "normal"}
                        onChange={(e) => updatePage({ difficulty: e.target.value })}
                      >
                        <option value="easy">easy (low effort)</option>
                        <option value="normal">normal (default)</option>
                        <option value="hard">hard (mid tier / high effort)</option>
                      </select>
                    </label>
                    {draft.pages.length > 1 && (
                      <button
                        type="button"
                        className="icon danger-ico"
                        title="Remove unit"
                        onClick={() => {
                          const removedId = p.id;
                          const next = draft.pages
                            .filter((_, j) => j !== idx)
                            .map((item) => ({
                              ...item,
                              depends_on: (item.depends_on ?? []).filter((d) => d !== removedId),
                            }));
                          setDraft({ ...draft, pages: next });
                        }}
                      >
                        <Trash2 size={14} />
                      </button>
                    )}
                  </div>
                  <label className="workbench-field">
                    <span>Brief</span>
                    <textarea
                      rows={2}
                      value={p.brief}
                      onChange={(e) => updatePage({ brief: e.target.value })}
                    />
                  </label>
                  <label className="workbench-field">
                    <span>Done criteria</span>
                    <input
                      value={p.done ?? ""}
                      onChange={(e) => updatePage({ done: e.target.value })}
                      placeholder="What must exist and pass?"
                    />
                  </label>
                  {otherIds.length > 0 && (
                    <div className="workbench-deps">
                      <span className="muted small">Depends on:</span>
                      {otherIds.map((oid) => {
                        const checked = (p.depends_on ?? []).includes(oid);
                        return (
                          <label key={oid} className={`toggle-chip small ${checked ? "on" : ""}`}>
                            <input type="checkbox" checked={checked} onChange={() => toggleDep(oid)} />
                            {oid}
                          </label>
                        );
                      })}
                    </div>
                  )}
                </div>
              );
            })}
          </div>

          {draft.contracts.length > 0 && (
            <div className="workbench-contracts">
              <h4>Shared Contracts ({draft.contracts.length})</h4>
              {draft.contracts.map((c: ContractFile, cIdx: number) => (
                <div key={cIdx} className="workbench-contract-card">
                  <input
                    className="mono"
                    value={c.path}
                    onChange={(e) => {
                      const next = draft.contracts.map((item, j) => (j === cIdx ? { ...item, path: e.target.value } : item));
                      setDraft({ ...draft, contracts: next });
                    }}
                  />
                  <textarea
                    className="mono"
                    rows={4}
                    value={c.content}
                    onChange={(e) => {
                      const next = draft.contracts.map((item, j) => (j === cIdx ? { ...item, content: e.target.value } : item));
                      setDraft({ ...draft, contracts: next });
                    }}
                  />
                </div>
              ))}
            </div>
          )}
        </section>
      )}
    </div>
  );
}
