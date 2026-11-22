// The daemon's HTTP API, typed after the Rust structs that produce it
// (crates/tm-daemon/src/api.rs and the tm-core types it serialises).

export type Phase = "planning" | "awaiting_approval" | "building" | "reviewing" | "verifying" | "done" | "failed";
export type PageState = "queued" | "building" | "flagged" | "passed" | "failed";
export type Tier = "worker" | "mid" | "master";
export type RoleName = "worker" | "mid" | "master" | "reviewer" | "verifier";

export interface Estimate {
  /** Fresh tokens: uncached input and output. */
  tokens: number;
  usd: number;
  /** Input expected from the provider's cache, counted apart. */
  cache_read?: number;
  /** Whether `usd` is a price at all; without one the estimate is unpriced, not $0.00. */
  priced?: boolean;
}

export interface Spend {
  calls: number;
  tokens: number;
  input: number;
  output: number;
  cache_read: number;
  cache_write: number;
  cost_usd: number;
  unpriced_calls: number;
  seconds: number;
}

export interface Totals {
  calls: number;
  input: number;
  output: number;
  cache_read: number;
  cache_write: number;
  tokens: number;
  fresh: number;
  cost_usd: number;
  unpriced_calls: number;
  seconds: number;
  by_role: Record<string, Spend>;
  by_model: Record<string, Spend>;
  /** `master`, `page:<id>`, or `<role> page:<id>`. */
  by_agent?: Record<string, Spend>;
}

export interface PageView {
  id: string;
  route: string | null;
  state: PageState;
  tier: Tier;
  repairs: number;
}

export interface ContractFile {
  path: string;
  content: string;
}

export interface PagePlan {
  id: string;
  route: string;
  owned_glob: string;
  cites: string[];
  brief: string;
  /** Self-contained context handed by the planner to this unit's worker. */
  context?: string;
  /** How the page's agent knows it is finished. */
  done?: string;
  /** Optional upstream worker/page IDs that must finish before this worker starts. */
  depends_on?: string[];
  units?: { id: string; owned_glob: string; brief: string; context?: string; done?: string; depends_on?: string[] }[];
  /** Skills from the store the planner attached; the page's agents read each SKILL.md. */
  skills?: string[];
  /** The phase of the sketch this unit belongs to, from 0. */
  phase?: number;
  /** Shown in a browser even though the plan is not web (true), or not although it is (false). */
  ui?: boolean;
  /** Difficulty hint (`easy`, `normal`, `hard`) used for model tier and effort routing. */
  difficulty?: string;
}

/** One agent a plan starts. Mirrors `Plan::agents` in tm-core. */
export interface AgentPlan {
  id: string;
  page: string;
  owned_glob: string;
  task: string;
  done: string;
}

const DEFAULT_DONE = "every file you own is written and does its part of the brief";

/** The build agents a plan starts: one per unit of a split page, else one per page. */
export function planAgents(plan: Plan): AgentPlan[] {
  return plan.pages.flatMap((page) => {
    const units = page.units ?? [];
    const split = units.length >= 2 && units.length <= 16;
    const list = split ? units : [{ id: page.id, owned_glob: page.owned_glob, brief: page.brief, done: page.done }];
    return list.map((u) => ({
      id: split ? `page:${page.id}/${u.id}` : `page:${page.id}`,
      page: page.id,
      owned_glob: u.owned_glob,
      task: u.brief.trim(),
      done: u.done?.trim() || DEFAULT_DONE,
    }));
  });
}

/** One phase of a run's rough sketch (PROJECT.md). */
export interface PhaseSketch {
  id: string;
  goal: string;
  delivers?: string[];
  interfaces?: string;
  test?: string;
  needs?: string;
}

/** One compact research decision recorded by the planner in PROJECT.md. */
export interface ResearchNote {
  decision: string;
  source: string;
  version: string;
}

export interface Plan {
  kind?: "web" | "library" | "cli" | "server" | "migration";
  /** Concise overview of the overall plan and worker division. */
  summary?: string;
  contracts: ContractFile[];
  pages: PagePlan[];
  estimate: Estimate;
  /** The planner's design spec every agent follows, when anything is shown in a browser. */
  design?: string;
  /** The sketch, when the run is built a phase at a time. */
  phases?: PhaseSketch[];
  /** Compact decisions from the planner's research phase. */
  research?: ResearchNote[];
}

export type Roles = Record<RoleName, string>;
export type Models = Partial<Record<RoleName, string | null>>;

export interface RunSummary {
  id: string;
  phase: Phase;
  /** Number of parallel worker slots configured for this run. */
  parallel?: number;
  active: boolean;
  /** The run was cancelled. Its phase is `failed`. */
  cancelled: boolean;
  error: string | null;
  /** A quota, rate-limit, or sign-in stop, when the daemon classified one. */
  limit?: RunLimit | null;
  branch: string | null;
  merged: boolean | null;
  /** The branch the run was merged into, and the short merge commit. */
  base?: string | null;
  merge_commit?: string | null;
  /** Set once the merge was taken back out: the undo commit, when, and how many files. */
  undone?: { commit: string; at: string; files: number } | null;
  pages: PageView[];
  estimate: Estimate | null;
  spent: Totals;
  review_rounds: number;
  amendments: number;
  /** Risky tool calls waiting for a person. */
  pending_approvals?: number;
  /** The first line of the prompt, sent with the list so a card need not fetch the run. */
  prompt_head?: string | null;
  /** First and last ledger timestamps, for the sidebar's clocks. */
  started_at?: string | null;
  last_at?: string | null;
}

export interface RunLimit {
  provider: string;
  role: string;
  kind: "quota" | "rate_limit" | "auth" | "balance";
  reset?: string | null;
  raw: string;
}

export type CancelMode = "graceful" | "force";

export interface RunDetail extends RunSummary {
  plan: Plan | null;
  /** The phase being built, from 0, and the one being planned after it passed. */
  phase_at?: number;
  next_phase?: number | null;
  prompt: string | null;
  /** How the run was asked for. Runs from before modes are `auto`. */
  mode?: RunMode;
  roles: Roles;
  models: Models;
  /** Active cancellation mode while the run is stopping. */
  cancelling?: CancelMode | null;
  /** Per-run USD spend cap, when configured. */
  budget_usd?: number | null;
}

export interface Flag {
  page: string;
  key: string;
  issue: string;
}

export interface Finding {
  page: string;
  key: string | null;
  file: string;
  line: number;
  failure: string;
}

export type WorkerOutcome = "passed" | { check_failed: { errors: string } } | { flagged: Flag } | { blocked: { reason: string } };

// Serde's externally tagged enum: a bare string for a unit variant, else one key.
export type RunEvent =
  | "plan_approved"
  | "plan_rejected"
  | "cancelled"
  | "resumed"
  | "follow_up"
  | { plan_ready: Plan }
  | { agent_turn: { plan: Plan } }
  | { worker_finished: { page: string; outcome: WorkerOutcome } }
  | { flag_resolved: { page: string; key: string; resolution: "conform" | "amended" } }
  | { review_finished: { findings: Finding[] } }
  | { findings_verified: { confirmed: Finding[] } }
  | { verify_finished: { failing: Record<string, string>; ok: boolean } }
  | { page_fresh: { page: string } };

export interface LedgerEntry {
  at: string;
  role: string;
  page: string | null;
  harness: string;
  model: string | null;
  input: number;
  output: number;
  cache_read: number;
  cache_write: number;
  cost_usd: number | null;
  seconds: number;
  ok: boolean;
  /** Tool calls the gate refused in this call. */
  refused?: string[];
  /** The agent that made the call (`page:<id>`, `unit:<id>/<part>`), on rows that have it. */
  agent?: string | null;
  unit?: string | null;
}

export type ChangeKind =
  | "contracts_locked"
  | "shared_repaired"
  | "page_built"
  | "page_repaired"
  | "entry_amended"
  | "review_finding"
  | "verify_failed";

export interface ChangeEntry {
  at: string;
  kind: ChangeKind;
  page: string | null;
  commit: string | null;
  model: string | null;
  files: string[];
  keys: string[];
  summary: string;
}

export interface Attempt {
  at: string;
  tier: Tier;
  model: string;
  outcome: "merged" | "discarded" | "flagged";
  fixing: string | null;
  failure: string | null;
  files: string[];
  said: string;
  diff: string | null;
}

export interface ModelChoice {
  id: string;
  name: string;
  default: boolean;
}

export interface Provider {
  id: string;
  name: string;
  installed: boolean;
  install: string;
  login: string;
  models: ModelChoice[];
  error: string | null;
}

export interface ProviderStatus {
  id: string;
  name: string;
  installed: boolean;
  path: string | null;
  version: string | null;
  /** null when the CLI cannot say without starting a session. */
  signed_in: boolean | null;
  account: string | null;
  latest: string | null;
  update_available: boolean | null;
  missing_runtimes: string[];
  notes: string[];
}

export interface RuntimeStatus {
  id: string;
  name: string;
  installed: boolean;
  version: string | null;
  used_for: string;
}

export type SetupAction = "install" | "update" | "login";
export type JobState = "running" | "done" | "failed" | "cancelled";

export interface SetupJob {
  id: string;
  kind: "provider" | "runtime";
  target: string;
  action: SetupAction;
  state: JobState;
  steps: { label: string; command: string; state: "waiting" | "running" | "done" | "failed" }[];
  output: string;
  prompt: { url: string | null; code: string | null };
  accepts_input: boolean;
  exit_code: number | null;
  error: string | null;
  started: string;
  finished: string | null;
}

export type TerminalKey = "enter" | "up" | "down" | "left" | "right" | "tab" | "escape" | "backspace" | "interrupt";

export interface EntryStats {
  reuses: number;
  pass_rate: number;
  passes: number;
  fails: number;
}

export interface DictionaryEntry {
  key: string;
  tier: "local" | "commons";
  kind: "schema" | "routes" | "component_api" | "pattern" | "hint";
  summary: string;
  content: string;
  hash: string;
  stack: string;
  stats: EntryStats | null;
}

export interface Mistake {
  signature: string;
  sample: string;
  count: number;
  fix: string | null;
}

export interface Book {
  models: Record<string, Mistake[]>;
}

/** One harness/model's record across recent runs, from the cost ledger. */
export interface ModelScore {
  key: string;
  harness: string;
  model?: string;
  calls: number;
  ok_calls: number;
  units_built: number;
  first_pass_units: number;
  first_pass_rate: number;
  total_tokens: number;
  cache_read: number;
  cache_hit_rate: number;
  total_usd: number;
  avg_usd_per_call: number;
  priced_calls: number;
  avg_seconds: number;
}

export interface ModelScorecard {
  runs_sampled: number;
  models: ModelScore[];
  recommended_worker?: string;
  /** Builders that pass under half their units first time, over 3 units or more. */
  struggling?: string[];
}

/** A mistake seen this often is put in front of the model that keeps making it. */
export const PROMOTE_AT = 3;

export interface Judged extends Finding {
  outcome: "confirmed" | "rejected";
  why: string | null;
}

export interface ReviewReport {
  at: string;
  base: string;
  head: string;
  dirty: boolean;
  reviewer: string;
  verifier: string;
  reviewed: boolean;
  findings: Judged[];
  dropped: number;
  duplicates: number;
}

export interface ReviewSummary {
  id: string;
  status: "running" | "done" | "failed";
  at: string;
  target: string;
  confirmed: number | null;
  run?: string | null;
}

export interface ReviewJob {
  id: string;
  status: "running" | "done" | "failed";
  at: string;
  target: string;
  reviewer: string;
  verifier: string;
  run?: string | null;
  text?: string;
  report?: ReviewReport;
  error?: string;
}

export interface Health {
  ok: boolean;
  version: string;
  auth: { methods: ("pairing" | "account")[]; sign_in?: string | null };
  /** False where the daemon has no terminal (Windows, for now). Older daemons leave it out. */
  terminal?: boolean;
}

/** Who does the work: the master and its workers, one agent, or the daemon's choice. */
export type RunMode = "auto" | "code" | "agent";

export interface NewRun {
  mode?: RunMode;
  prompt?: string;
  id?: string;
  yes?: boolean;
  roles?: Partial<Roles>;
  models?: Models;
  parallel?: number;
  timeout_secs?: number;
  template?: string;
  max_cost_usd?: number;
}

export interface NewProject {
  mode?: RunMode;
  name: string;
  prompt?: string;
  yes?: boolean;
  roles?: Partial<Roles>;
  models?: Models;
  parallel?: number;
  timeout_secs?: number;
  template?: string;
  max_cost_usd?: number;
  switch?: boolean;
}

export interface NewReview {
  target?: string;
  base?: string;
  head?: string;
  pr?: number;
  dirty?: boolean;
  reviewer?: string;
  verifier?: string;
  reviewer_model?: string;
  verifier_model?: string;
}

/** Who an agent transcript entry is from. */
export interface AgentTag {
  id: string;
  role: string;
  page?: string | null;
  harness?: string;
  model?: string | null;
  owned_glob?: string | null;
  /** The work unit, when a page is split across parallel workers. */
  unit?: string | null;
  worktree?: string;
  branch?: string | null;
  call?: number;
}

export interface AgentEntry {
  at: string;
  agent: AgentTag;
  kind:
    | "start"
    | "text"
    | "thought"
    | "tool_call"
    | "permission"
    | "end"
    | "command"
    | "output"
    | "preview"
    | "approval_requested"
    | "approval_resolved"
    | "stalled"
    | "released"
    | "restarted"
    | "turn"
    | "web_search"
    | "web_fetch"
    | "docs"
    | "advisories";
  /** Tool call id, on a `tool_call` and on the `output` that belongs to it. */
  id?: string;
  text?: string;
  /** Unified diff attached to an `output`, when the harness sent one. */
  diff?: string;
  brief?: string;
  name?: string;
  target?: string | null;
  input?: unknown;
  tool?: string;
  allowed?: boolean;
  ok?: boolean;
  usage?: { input: number; output: number; cache_read: number; cache_write: number };
  cost_usd?: number | null;
  seconds?: number;
  error?: string;
  /** On an `end` the daemon wrote for a call that was cut off by a cancel or a stop. */
  stopped?: boolean;
  /** On a person's `turn`: which turn of the run it opens (the first is 1), and who takes it. */
  turn?: number;
  mode?: RunMode;
  /** On a person's `turn`: the ids of the notes it took. */
  notes?: string[];
  command?: string;
  cwd?: string;
  typed?: boolean;
  code?: number | null;
  step?: string;
  routes?: { page: string; route: string }[];
  /** `released`: the agent asked for it, and what was freed. */
  asked?: boolean;
  processes?: number;
  lingering?: number;
  /** `stalled`: which fresh agent this is. */
  restart?: number;
  /** Phase 1 web access transcript fields (`web_search`, `web_fetch`, `docs`). */
  provider?: string;
  query?: string;
  hits?: number;
  sources?: { title: string; url: string; engine?: string }[];
  url?: string;
  final_url?: string;
  host?: string;
  bytes?: number;
  cached?: boolean;
  package?: string;
  version?: string;
  ecosystem?: string;
  symbol?: string | null;
  source?: string;
  path?: string;
  injection_flagged?: boolean;
  /** P5: oversized (>2 KiB) output body omitted from the wire; fetch via `GET /api/runs/{id}/agents/{body_index}`. */
  body_omitted?: boolean;
  body_index?: number;
  text_bytes?: number;
  diff_bytes?: number;
}

export interface OlderCallSummary {
  agent: AgentTag;
  call: number;
  start_index: number;
  end_index: number;
  entries: number;
  ok: boolean | null;
  seconds: number | null;
}

export interface CommandOutputFrame {
  mode: "append" | "replace";
  chunk?: string;
  text?: string;
  next_offset: number;
  bytes: number;
  truncated?: boolean;
}

export interface Note {
  id: string;
  to: string;
  text: string;
  at: string;
  delivered: boolean;
  mode?: RunMode;
}

export interface DaemonSettings {
  project: string;
  /** Parent directory where new projects are created. */
  workspace?: string;
  /** Checked-out branch of the project, when it is a git repository. */
  branch?: string | null;
  dictionary: string;
  commons: string;
  commons_remote: boolean;
  config: string | null;
  config_default: string;
  store: string;
  /** Only the roles the person chose. A role missing here has no provider yet. */
  roles: Partial<Roles>;
  models?: Models;
  /** What this machine can run at once. Workers always run here. */
  machine?: { cpus: number; free_memory_gb: number | null; max_parallel: number };
  version: string;
  approvals?: { mode: ApprovalMode; timeout_secs: number };
  checker?: { enabled: boolean };
  code_mode?: { enabled: boolean };
  /** The workers' sandbox: a shell inside it, and fetches to the package indexes. */
  sandbox?: { shell: boolean; package_indexes: boolean; available: boolean };
  telemetry?: { enabled: boolean };
  /** Seconds one agent call may run; 0 is no limit. */
  agents?: { timeout_secs: number; budget_usd?: number };
}

export type ApprovalMode = "always" | "risky" | "never";

/** One risky tool call an agent is waiting on. */
export interface ToolApproval {
  id: string;
  run: string;
  agent: unknown;
  tool: string;
  kind: "read" | "edit" | "execute" | "fetch" | "dictionary" | "other";
  command: string | null;
  paths: string[];
  reason: string;
  at: string;
  timeout_secs: number;
}

export interface PreviewView {
  available: boolean;
  state: "none" | "starting" | "running" | "stopped" | "failed";
  url?: string | null;
  port?: number | null;
  command?: string | null;
  reason?: string | null;
  output?: string | null;
  started?: string | null;
}

export interface EditorInfo {
  id: string;
  name: string;
  command: string;
  installed: boolean;
}

export interface Skill {
  name: string;
  description: string;
  /** store: in Taskmaster's store. project / home: found on disk, can be copied in. */
  place: "store" | "project" | "home";
  path: string;
  enabled: boolean;
  source: { repo: string; path: string; tree: string } | null;
  installed: string | null;
  updated: string | null;
}

export interface SkillHit {
  id: string;
  name: string;
  repo: string;
  skill: string;
  installs: number;
  installed: boolean;
}

export interface GhStatus {
  installed: boolean;
  account: string | null;
  error?: string;
}

export interface HostConnection {
  id: string;
  kind: string;
  name: string;
  base: string;
  account: string | null;
  connected: boolean;
  via: "cli" | "token" | null;
  error?: string | null;
}

export interface GitRemote {
  name: string;
  url: string;
}

export interface GitState {
  repo: boolean;
  branch: string | null;
  detached: boolean;
  dirty: boolean;
  remotes: GitRemote[];
}

export interface GrepHit {
  path: string;
  line: number;
  column: number;
  text: string;
}

export interface ProjectEntry {
  name: string;
  path: string;
  current: boolean;
}

export interface ProjectConfigData {
  setup?: string;
  preview?: string;
  test?: string;
  shared?: string[];
  copy?: string[];
  archive?: string;
}

export interface ProjectConfigPayload {
  exists: boolean;
  path: string;
  raw?: string;
  config?: ProjectConfigData;
  hash?: string;
  has_commands: boolean;
  trusted: boolean;
  error?: string;
}

export interface TerminalInfo {
  id: string;
  cwd: string;
  shell: string;
  started: string;
  alive: boolean;
  code: number | null;
}

// Where the daemon is, and the token it gave this device. Kept in localStorage so a
// phone that paired once stays paired.

const SERVER_KEY = "taskmaster.server";
const TOKEN_KEY = "taskmaster.token";

export function server(): string {
  return localStorage.getItem(SERVER_KEY) ?? "";
}

export function setServer(url: string) {
  const clean = url.trim().replace(/\/+$/, "");
  if (clean) localStorage.setItem(SERVER_KEY, clean);
  else localStorage.removeItem(SERVER_KEY);
}

export function token(): string | null {
  return localStorage.getItem(TOKEN_KEY);
}

export function setToken(value: string | null) {
  if (value) localStorage.setItem(TOKEN_KEY, value);
  else localStorage.removeItem(TOKEN_KEY);
  window.dispatchEvent(new Event("taskmaster-auth"));
}

export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
    readonly body: unknown = null,
  ) {
    super(message);
  }
}

export type DeviceKind = "browser" | "phone" | "desktop" | "machine" | "agent";

/** One way to reach the daemon: this computer, the local network, the tailnet. */
export interface ConnectRoute {
  kind: "this-computer" | "local-network" | "tailscale" | "network";
  address: string;
  /** Ends in `/`. */
  url: string;
}

export interface ConnectView {
  version: string;
  bind: string;
  /** Only this computer can reach the daemon: it listens on loopback. */
  only_this_computer: boolean;
  routes: ConnectRoute[];
  tailscale: { running: boolean; ip: string | null; dns: string | null };
  mcp_url: string;
  project: string;
  kinds: DeviceKind[];
}

export interface PairingOffer {
  code: string;
  label: string;
  kind: DeviceKind | "";
  expires_at: string;
  links: ConnectRoute[];
}

export interface Device {
  id: string;
  label: string;
  kind: DeviceKind;
  created: string;
  last_seen: string | null;
  current: boolean;
}

/** What this browser calls itself when it pairs: "Chrome on Android", "Firefox on Linux". */
export function deviceLabel(ua = navigator.userAgent): { label: string; kind: DeviceKind } {
  const os = /Android/i.test(ua) ? "Android" : /iPhone|iPad/i.test(ua) ? "iOS" : /Mac OS X/i.test(ua) ? "macOS" : /Windows/i.test(ua) ? "Windows" : /Linux/i.test(ua) ? "Linux" : "";
  const browser = /Edg\//.test(ua) ? "Edge" : /Firefox\//.test(ua) ? "Firefox" : /Chrome\//.test(ua) ? "Chrome" : /Safari\//.test(ua) ? "Safari" : "Browser";
  const phone = /Android|iPhone|iPad/i.test(ua);
  if ((window as unknown as { __TAURI__?: unknown }).__TAURI__) return { label: `Desktop app${os ? ` on ${os}` : ""}`, kind: "desktop" };
  return { label: os ? `${browser} on ${os}` : browser, kind: phone ? "phone" : "browser" };
}

/** A pairing link's parts: the daemon's address and the one-time code after `#code=`. */
export function parsePairingLink(text: string): { base: string; code: string } | null {
  try {
    const url = new URL(text.trim());
    const code = new URLSearchParams(url.hash.slice(1)).get("code") ?? url.searchParams.get("code");
    if (!code || !/^https?:$/.test(url.protocol)) return null;
    return { base: url.origin, code: code.trim() };
  } catch {
    return null;
  }
}

/** Pair with another daemon from its link; returns its address and the token it gave. */
export async function pairAt(link: string, label: string): Promise<{ url: string; token: string }> {
  const parsed = parsePairingLink(link);
  if (!parsed) throw new Error("That is not a pairing link. It looks like http://host:8737/pair#code=…");
  let response: Response;
  try {
    response = await fetch(`${parsed.base}/api/pair`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ code: parsed.code, label, kind: "machine" }),
    });
  } catch {
    throw new Error(`Cannot reach ${parsed.base}. Is it on this network, and listening there (tm-daemon serve --lan)?`);
  }
  const body = (await response.json().catch(() => null)) as { token?: string; error?: string } | null;
  if (!response.ok || !body?.token) throw new Error(body?.error ?? "That link was refused: it may be used or expired. Ask for a new one.");
  return { url: parsed.base, token: body.token };
}

/** Call another daemon with its own address and token (for gallery cards of other daemons). */
export async function callAt<T>(base: string, bearer: string | null, path: string): Promise<T> {
  const response = await fetch(base.replace(/\/+$/, "") + path, {
    headers: bearer ? { authorization: `Bearer ${bearer}` } : {},
  });
  if (!response.ok) throw new ApiError(response.status, `${response.status} ${response.statusText}`);
  return (await response.json()) as T;
}

/**
 * Whether the daemon answered the last request. The connection banner listens for
 * `taskmaster-connection` and probes `/api/health` until it answers again.
 */
let reachable = true;
export function daemonReachable(): boolean {
  return reachable;
}
function setReachable(next: boolean) {
  if (next === reachable) return;
  reachable = next;
  window.dispatchEvent(new CustomEvent("taskmaster-connection", { detail: next }));
}

async function call<T>(method: "GET" | "POST" | "DELETE", path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = {};
  const bearer = token();
  if (bearer) headers.authorization = `Bearer ${bearer}`;
  if (body !== undefined) headers["content-type"] = "application/json";
  let response: Response;
  try {
    response = await fetch(server() + path, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
    });
  } catch {
    setReachable(false);
    throw new ApiError(0, `cannot reach the daemon${server() ? ` at ${server()}` : ""}`);
  }
  setReachable(true);
  const text = await response.text();
  const data = text ? JSON.parse(text) : null;
  if (!response.ok) {
    if (response.status === 401 && bearer) setToken(null);
    throw new ApiError(response.status, data?.error ?? `${response.status} ${response.statusText}`, data);
  }
  return data as T;
}

const enc = encodeURIComponent;

export interface SearchHit {
  title: string;
  url: string;
  content: string;
  engine: string;
}

export interface DiffStat {
  added: number;
  removed: number;
  files: number;
}

export interface DiffStats {
  pages: Record<string, DiffStat>;
  total: DiffStat;
}

export interface LanguageStat {
  language: string;
  files: number;
  lines: number;
}

export interface ModuleCluster {
  name: string;
  files: number;
  lines: number;
  exports: number;
  tests: number;
}

export interface HubFile {
  path: string;
  imports_in: number;
  exports: number;
  lines: number;
}

export interface ChurnFile {
  path: string;
  touches: number;
}

export interface TestLink {
  module: string;
  test_files: string[];
}

export interface Atlas {
  head: string | null;
  generated_at: string;
  total_files: number;
  total_lines: number;
  languages: LanguageStat[];
  modules: ModuleCluster[];
  hubs: HubFile[];
  hotspots: ChurnFile[];
  test_map: TestLink[];
  conventions: string[];
}

export type CheckStatus = "ok" | "warn" | "fail";

export interface DoctorCheck {
  id: string;
  label: string;
  category: string;
  status: CheckStatus;
  detail: string;
  fix: string | null;
}

export interface DoctorReport {
  ok: boolean;
  summary: string;
  checks: DoctorCheck[];
  checked_at: string;
}

export interface Policy {
  protected_globs: string[];
  denied_globs: string[];
  allowed_fetch_hosts: string[];
  denied_fetch_hosts: string[];
  max_run_cost_usd: number | null;
  daily_budget_usd: number | null;
  require_cross_family_review: boolean;
  redact_patterns: string[];
}

export interface PolicyResponse {
  policy: Policy;
  source: string | null;
  toml: string;
}

export interface Checkpoint {
  sha: string;
  short_sha: string;
  kind: string;
  label: string;
  ref_name?: string;
  page?: string;
  model?: string;
  at?: string;
}

export interface ForkRunBody {
  id?: string;
  prompt?: string;
  plan?: Plan;
  yes?: boolean;
  parallel?: number;
  max_cost_usd?: number;
  note?: string;
}

export interface RunBundle {
  format: string;
  exported_at: string;
  run: RunSummary;
  plan: Plan | null;
  events: RunEvent[];
  ledger: LedgerEntry[];
  changes: ChangeEntry[];
  transcript: AgentEntry[];
  checkpoints: Checkpoint[];
  cache_economics: {
    total_input_tokens: number;
    fresh_input_tokens: number;
    cached_read_tokens: number;
    cache_write_tokens: number;
    output_tokens: number;
    cache_hit_rate: number;
    total_cost_usd: number;
    by_role: Record<string, { calls: number; cost_usd: number; input: number; cache_read: number; cache_write: number; output: number; seconds: number }>;
  };
}

export interface MonthlySpendReport {
  current_month: string;
  current_month_usd: number;
  alert_threshold_usd?: number | null;
  alert_triggered: boolean;
  by_month: Record<string, number>;
  total_runs: number;
  total_calls: number;
}

export interface EgressEntry {
  at: string;
  run_id?: string | null;
  host: string;
  url: string;
  allowed: boolean;
  reason?: string | null;
}

export interface GcReport {
  removed_runs: string[];
  kept_runs: number;
  worktrees_pruned: boolean;
  bytes_freed: number;
}

export interface RemoteNode {
  id: string;
  name: string;
  url: string;
  kind: string;
  labels: string[];
  max_parallel: number;
  enabled: boolean;
  has_token: boolean;
}

export interface NodeHealth {
  id: string;
  reachable: boolean;
  latency_ms: number;
  version?: string | null;
  error?: string | null;
}

export interface UserMcpServer {
  name: string;
  command: string;
  args: string[];
  env: Record<string, string>;
  secret_env_keys: string[];
  roles: string[];
  enabled: boolean;
  require_approval: boolean;
}

export interface NotificationConfig {
  desktop: boolean;
  webhook_url?: string | null;
  on_approval: boolean;
  on_limit: boolean;
  on_finish: boolean;
}

export interface ScheduledRun {
  id: string;
  name: string;
  schedule: string;
  prompt: string;
  enabled: boolean;
  auto_approve: boolean;
  max_cost_usd?: number | null;
  last_run_at?: string | null;
  last_run_id?: string | null;
}

export interface IssueParseResult {
  issue: {
    host?: string;
    owner?: string;
    repo?: string;
    number?: number;
    url?: string;
    title?: string;
    body?: string;
    labels?: string[];
    issue?: { host: string; owner: string; repo: string; number: number; url: string };
  };
  prompt?: string;
  closing_link: string;
}

export interface AttachmentMeta {
  id: string;
  name: string;
  mime: string;
  bytes: number;
  relative_path: string;
}

export interface CiCheck {
  name: string;
  state: string;
  detail?: string | null;
  url?: string | null;
}

export interface PrSyncReport {
  checks: CiCheck[];
  failing_checks: number;
  comments_synced: number;
  queued_notes: { target: string; text: string }[];
}

export interface StratifiedSlice {
  difficulty: string;
  language: string;
  models: ModelScore[];
  recommended?: string | null;
}

export interface EvalSuiteTask {
  id: string;
  category: string;
  language: string;
  difficulty: string;
  prompt: string;
  hidden_test_file: string;
}

export interface WebRolePolicy {
  search: boolean;
  fetch: boolean;
}

export interface WebRoleMatrix {
  planner: WebRolePolicy;
  worker: WebRolePolicy;
  reviewer: WebRolePolicy;
  verifier: WebRolePolicy;
}

export interface WebAccessSettings {
  enabled: boolean;
  provider: string;
  endpoint: string;
  roles: WebRoleMatrix;
  allowlist: string[];
  planner_max_searches: number;
  planner_max_fetches: number;
  max_fetch_bytes: number;
  cache_ttl_secs: number;
  cache_max_mb: number;
}

export interface WebCacheStats {
  dir: string;
  entries: number;
  bytes: number;
}

export interface WebAccessPayload {
  web_access: WebAccessSettings;
  has_api_key: boolean;
  providers_with_keys?: string[];
  vault_backend: string;
  derived_allowlist: string[];
  advisory_hosts: string[];
  providers: string[];
  cache: WebCacheStats;
}

export interface WebAccessUpdate {
  enabled?: boolean;
  provider?: string;
  endpoint?: string;
  api_key?: string;
  clear_api_key?: boolean;
  roles?: WebRoleMatrix;
  allowlist?: string[];
  planner_max_searches?: number;
  planner_max_fetches?: number;
  max_fetch_bytes?: number;
  cache_ttl_secs?: number;
  cache_max_mb?: number;
}

export interface WebActivityItem {
  at: string;
  agent: AgentTag;
  kind: "web_search" | "web_fetch" | "docs" | "advisories";
  provider?: string;
  query?: string;
  hits?: number;
  sources?: { title: string; url: string; engine?: string }[];
  url?: string;
  final_url?: string;
  host?: string;
  bytes?: number;
  cached?: boolean;
  package?: string;
  version?: string | null;
  ecosystem?: string;
  symbol?: string | null;
  source?: string;
  path?: string;
  count?: number;
  advisories?: { id: string; aliases?: string[]; summary?: string }[];
  injection_flagged?: boolean;
}

export function searchWeb(q: string, endpoint: string) {
  const params = new URLSearchParams({ q });
  if (endpoint) params.set("endpoint", endpoint);
  return call<{ endpoint: string; results: SearchHit[] }>("GET", `/api/search?${params}`);
}

export const api = {
  health: () => call<Health>("GET", "/api/health"),
  pair: (code: string, label?: string, kind?: DeviceKind) => call<{ token: string }>("POST", "/api/pair", { code, label: label ?? "", kind: kind ?? "" }),
  connect: () => call<ConnectView>("GET", "/api/connect"),
  createPairing: (body: { label?: string; kind?: DeviceKind; ttl_secs?: number }) => call<PairingOffer>("POST", "/api/pairing", body),
  devices: () => call<{ devices: Device[] }>("GET", "/api/devices").then((r) => r.devices),
  renameDevice: (id: string, label: string) => call<{ ok: boolean }>("POST", `/api/devices/${enc(id)}/rename`, { label }),
  revokeDevice: (id: string) => call<{ ok: boolean }>("POST", `/api/devices/${enc(id)}/revoke`, {}),
  mintAgent: (label: string) => call<{ token: string; id: string; mcp_url: string }>("POST", "/api/devices/agent", { label }),

  runs: () => call<{ runs: RunSummary[]; seq?: number }>("GET", "/api/runs").then((r) => r.runs),
  runsDelta: (since?: number) =>
    call<{ runs: RunSummary[]; seq?: number; all_ids?: string[]; delta?: boolean }>(
      "GET",
      since !== undefined ? `/api/runs?since=${since}` : "/api/runs",
    ),
  run: (id: string) => call<RunDetail>("GET", `/api/runs/${enc(id)}`),
  createRun: (body: NewRun) => call<{ id: string }>("POST", "/api/runs", body),
  createProject: (body: NewProject) => call<{ project: string; id?: string }>("POST", "/api/projects", body),
  events: (id: string, after = 0) =>
    call<{ next: number; events: RunEvent[] }>("GET", `/api/runs/${enc(id)}/events?after=${after}`),
  ledger: (id: string, after?: number) =>
    call<{ totals: Totals; entries: LedgerEntry[]; total_entries?: number; after?: number }>(
      "GET",
      after !== undefined ? `/api/runs/${enc(id)}/ledger?after=${after}` : `/api/runs/${enc(id)}/ledger`,
    ),
  changes: (id: string) => call<{ changes: ChangeEntry[] }>("GET", `/api/runs/${enc(id)}/changes`).then((r) => r.changes),
  attempts: (id: string, page: string) =>
    call<{ attempts: Attempt[] }>("GET", `/api/runs/${enc(id)}/attempts/${enc(page)}`).then((r) => r.attempts),
  screens: (id: string) => call<{ pages: string[] }>("GET", `/api/runs/${enc(id)}/screens`).then((r) => r.pages),
  /** A screenshot as an object URL. An <img> cannot send the token, so it is fetched. */
  screen: async (id: string, page: string) => {
    const bearer = token();
    const response = await fetch(`${server()}/api/runs/${enc(id)}/screens/${enc(page)}`, {
      headers: bearer ? { authorization: `Bearer ${bearer}` } : {},
    });
    if (!response.ok) throw new ApiError(response.status, "no screenshot");
    return URL.createObjectURL(await response.blob());
  },
  approve: (id: string, note?: string) => call("POST", `/api/runs/${enc(id)}/approve`, note ? { note } : {}),
  reject: (id: string, prompt?: string) => call("POST", `/api/runs/${enc(id)}/reject`, { prompt }),
  resume: (id: string) => call("POST", `/api/runs/${enc(id)}/resume`, {}),
  /** One commit that puts back every file a merged run changed. 409 says why it cannot. */
  undo: (id: string) => call<{ id: string; commit: string; files: number }>("POST", `/api/runs/${enc(id)}/undo`, {}),
  cancel: (id: string, mode?: CancelMode) =>
    call<{ id: string; mode: CancelMode }>("POST", `/api/runs/${enc(id)}/cancel`, mode ? { mode } : {}),
  restartWorker: (id: string, page: string) =>
    call<{ ok: boolean; mode: "restarted" | "queued"; page: string }>("POST", `/api/runs/${enc(id)}/workers/${enc(page)}/restart`, {}),
  updatePlan: (id: string, plan: Plan) => call<RunDetail>("POST", `/api/runs/${enc(id)}/plan`, { plan }),
  checkpoints: (id: string) =>
    call<{ checkpoints: Checkpoint[] }>("GET", `/api/runs/${enc(id)}/checkpoints`).then((r) => r.checkpoints),
  forkRun: (id: string, body?: ForkRunBody) =>
    call<{ id: string; forked_from: string }>("POST", `/api/runs/${enc(id)}/fork`, body ?? {}),
  bundle: (id: string) => call<RunBundle>("GET", `/api/runs/${enc(id)}/bundle`),

  atlas: (refresh = false) =>
    call<{ atlas: Atlas }>("GET", `/api/atlas${refresh ? "?refresh=1" : ""}`).then((r) => r.atlas),
  doctor: () => call<{ doctor: DoctorReport }>("GET", "/api/doctor").then((r) => r.doctor),
  policy: () => call<PolicyResponse>("GET", "/api/policy"),
  savePolicy: (policy: Policy, repoFile = false) =>
    call<{ ok: boolean } & PolicyResponse>("POST", "/api/policy", { policy, repo_file: repoFile }),

  /** The providers' model lists. The daemon answers from a recent list unless `fresh`. */
  providers: (harness?: string, fresh = false) => {
    const query = [harness ? `harness=${enc(harness)}` : "", fresh ? "fresh=1" : ""].filter(Boolean).join("&");
    return call<{ providers: Provider[] }>("GET", `/api/providers${query ? `?${query}` : ""}`).then((r) => r.providers);
  },
  setup: () => call<{ providers: ProviderStatus[]; runtimes: RuntimeStatus[]; jobs: SetupJob[] }>("GET", "/api/setup"),
  startProviderJob: (id: string, action: SetupAction) => call<SetupJob>("POST", `/api/setup/providers/${enc(id)}/${action}`, {}),
  installRuntime: (id: string) => call<SetupJob>("POST", `/api/setup/runtimes/${enc(id)}/install`, {}),
  job: (id: string) => call<SetupJob>("GET", `/api/setup/jobs/${enc(id)}`),
  jobInput: (id: string, body: { text?: string; key?: TerminalKey }) => call("POST", `/api/setup/jobs/${enc(id)}/input`, body),
  cancelJob: (id: string) => call<SetupJob>("POST", `/api/setup/jobs/${enc(id)}/cancel`, {}),
  templates: () => call<{ templates: string[] }>("GET", "/api/templates").then((r) => r.templates),
  dictionary: (q: string) =>
    call<{ entries: DictionaryEntry[] }>("GET", `/api/dictionary?q=${enc(q)}`).then((r) => r.entries),
  entry: (key: string) => call<DictionaryEntry>("GET", `/api/dictionary/${enc(key)}`),
  errorography: () => call<Book>("GET", "/api/errorography"),
  scorecard: () => call<ModelScorecard>("GET", "/api/scorecard"),

  agents: (id: string, after = 0, opts?: { window?: "recent"; before?: number; stub?: boolean }) => {
    const params = new URLSearchParams();
    if (opts?.window) params.set("window", opts.window);
    else if (opts?.before !== undefined) params.set("before", String(opts.before));
    else params.set("after", String(after));
    if (opts?.stub) params.set("stub", "1");
    return call<{
      next: number;
      entries: AgentEntry[];
      window_start?: number;
      window_end?: number;
      has_older?: boolean;
      older_calls?: OlderCallSummary[];
    }>("GET", `/api/runs/${enc(id)}/agents?${params}`);
  },
  agentEntry: (id: string, index: number) =>
    call<{ index: number; text: string | null; diff: string | null; entry: AgentEntry }>(
      "GET",
      `/api/runs/${enc(id)}/agents/${index}`,
    ),
  notes: (id: string) => call<{ notes: Note[] }>("GET", `/api/runs/${enc(id)}/notes`).then((r) => r.notes),
  addNote: (id: string, to: string, text: string, mode?: RunMode) =>
    call<Note>("POST", `/api/runs/${enc(id)}/notes`, mode ? { to, text, mode } : { to, text }),
  editNote: (id: string, note: string, text?: string) =>
    call<{ notes: Note[] }>("POST", `/api/runs/${enc(id)}/notes/${enc(note)}`, text ? { text } : {}),
  terminal: (id: string, command: string) =>
    call<{ ok: boolean; code: number | null; output: string }>("POST", `/api/runs/${enc(id)}/terminal`, { command }),
  diff: (id: string, scope: "run" | "page", page?: string) =>
    call<{ scope: string; base: string; branch: string; diff: string }>(
      "GET",
      `/api/runs/${enc(id)}/diff?scope=${scope}${page ? `&page=${enc(page)}` : ""}`,
    ),
  diffStats: (id: string) => call<DiffStats>("GET", `/api/runs/${enc(id)}/diff-stats`),
  draftPr: (id: string) => call<{ url: string }>("POST", `/api/runs/${enc(id)}/pr`, {}),
  approvals: (run: string) =>
    call<{ approvals: ToolApproval[]; mode: ApprovalMode; timeout_secs: number }>("GET", `/api/approvals?run=${enc(run)}`),
  approveTool: (id: string) => call<{ id: string; allowed: boolean }>("POST", `/api/approvals/${enc(id)}/approve`, {}),
  denyTool: (id: string) => call<{ id: string; allowed: boolean }>("POST", `/api/approvals/${enc(id)}/deny`, {}),
  /** Save role choices. Roles left out keep what was saved; an empty string forgets one. */
  setRoleDefaults: (body: { roles: Partial<Roles>; models?: Models }) => call<DaemonSettings>("POST", "/api/settings", body),
  setApprovals: (body: { approvals?: ApprovalMode; approval_timeout_secs?: number }) => call("POST", "/api/settings", body),
  setChecker: (enabled: boolean) => call<DaemonSettings>("POST", "/api/settings", { checker: enabled }),
  setCodeMode: (enabled: boolean) => call<DaemonSettings>("POST", "/api/settings", { code_mode: enabled }),
  setSandbox: (change: { shell?: boolean; package_indexes?: boolean }) => call<DaemonSettings>("POST", "/api/settings", { sandbox: change }),
  setTelemetry: (enabled: boolean) => call<DaemonSettings>("POST", "/api/settings", { telemetry: enabled }),
  setAgentTimeout: (secs: number) => call<DaemonSettings>("POST", "/api/settings", { agent_timeout_secs: secs }),
  logs: () => call<{ path: string | null; lines: string[] }>("GET", "/api/logs"),
  exportSettings: () => call<{ settings: Record<string, unknown>; note: string }>("GET", "/api/settings/export"),
  importSettings: (settings: unknown) => call<DaemonSettings>("POST", "/api/settings/import", { settings }),
  resetSettings: () => call<DaemonSettings>("POST", "/api/settings/reset", {}),
  preview: (id: string) => call<PreviewView>("GET", `/api/runs/${enc(id)}/preview`),
  startPreview: (id: string) => call<PreviewView>("POST", `/api/runs/${enc(id)}/preview`, {}),
  stopPreview: (id: string) => call<PreviewView>("POST", `/api/runs/${enc(id)}/preview/stop`, {}),
  settings: () => call<DaemonSettings>("GET", "/api/settings"),
  editors: () => call<{ editors: EditorInfo[]; default: string }>("GET", "/api/editors"),
  open: (body: { path: string; line?: number; column?: number; editor?: string; reveal?: boolean }) =>
    call<{ command: string; args: string[]; path: string }>("POST", "/api/open", body),
  paths: (paths: string[]) =>
    call<{ paths: Record<string, string | null>; project: string }>("POST", "/api/paths", { paths }),
  file: (path: string) =>
    call<{ path: string; full: string; content: string; truncated: boolean }>("GET", `/api/file?path=${enc(path)}`),
  files: () => call<{ files: string[] }>("GET", "/api/files").then((r) => r.files),
  grep: (q: string) => call<{ hits: GrepHit[]; truncated: boolean }>("GET", `/api/grep?q=${enc(q)}`),
  projects: () => call<{ projects: ProjectEntry[]; workspace: string }>("GET", "/api/projects"),
  openProject: (path: string) => call<{ project: string }>("POST", "/api/projects/open", { path }),
  projectConfig: () => call<ProjectConfigPayload>("GET", "/api/project-config"),
  trustProjectConfig: (hash?: string) => call<ProjectConfigPayload>("POST", "/api/project-config/trust", hash ? { hash } : {}),
  generateProjectConfig: (overwrite = false) => call<ProjectConfigPayload>("POST", "/api/project-config/generate", { overwrite }),
  terminals: () => call<{ terminals: TerminalInfo[] }>("GET", "/api/terminals").then((r) => r.terminals),
  openTerminal: (cols: number, rows: number) => call<TerminalInfo>("POST", "/api/terminals", { cols, rows }),
  terminalInput: (id: string, data: string) => call("POST", `/api/terminals/${enc(id)}/input`, { data }),
  resizeTerminal: (id: string, cols: number, rows: number) => call("POST", `/api/terminals/${enc(id)}/resize`, { cols, rows }),
  closeTerminal: (id: string) => call("POST", `/api/terminals/${enc(id)}/close`, {}),
  skills: () => call<{ skills: Skill[] }>("GET", "/api/skills").then((r) => r.skills),
  searchSkills: (q: string) => call<{ hits: SkillHit[] }>("GET", `/api/skills/search?q=${enc(q)}`).then((r) => r.hits),
  skillUpdates: () => call<{ updates: Record<string, string> }>("GET", "/api/skills/updates").then((r) => r.updates),
  installSkill: (repo: string, skill: string) => call<Skill>("POST", "/api/skills/install", { repo, skill }),
  enableSkill: (name: string, enabled: boolean) => call<Skill>("POST", `/api/skills/${enc(name)}/enable`, { enabled }),
  updateSkill: (name: string) => call<Skill>("POST", `/api/skills/${enc(name)}/update`, {}),
  removeSkill: (name: string) => call<{ removed: string }>("POST", `/api/skills/${enc(name)}/remove`, {}),
  importSkill: (name: string) => call<Skill>("POST", `/api/skills/${enc(name)}/import`, {}),
  gh: () => call<GhStatus>("GET", "/api/gh"),
  ghPr: (n: number) => call<{ number: number; title: string; url: string; state: string }>("GET", `/api/gh/pr/${n}`),
  hosts: () => call<{ hosts: HostConnection[] }>("GET", "/api/hosts").then((r) => r.hosts),
  connectHost: (body: { kind: string; token: string; base?: string; user?: string }) => call<HostConnection>("POST", "/api/hosts", body),
  forgetHost: (id: string) => call<{ ok: boolean }>("DELETE", `/api/hosts/${enc(id)}`),
  createHostRepo: (id: string, body: { name: string; private?: boolean; push?: boolean }) =>
    call<{ clone_url: string | null; html_url: string; pushed: boolean }>("POST", `/api/hosts/${enc(id)}/repos`, body),
  git: () => call<GitState>("GET", "/api/git"),
  setRemote: (body: { name: string; url: string }) => call<GitState>("POST", "/api/git/remote", body),
  pushGit: (body?: { remote?: string; branch?: string }) => call<{ remote: string; branch: string }>("POST", "/api/git/push", body ?? {}),
  cloneRepo: (body: { url: string; name?: string }) => call<{ path: string; name: string }>("POST", "/api/git/clone", body),

  reviews: () => call<{ reviews: ReviewSummary[] }>("GET", "/api/reviews").then((r) => r.reviews),
  review: (id: string) => call<ReviewJob>("GET", `/api/reviews/${enc(id)}`),
  reviewRun: (id: string) => call<{ id: string }>("POST", `/api/runs/${enc(id)}/review`, {}),
  cancelReview: (id: string) => call<{ id: string }>("POST", `/api/reviews/${enc(id)}/cancel`, {}),
  createReview: (body: NewReview) => call<{ id: string }>("POST", "/api/reviews", body),

  // Phase 2-5 operations & workflow endpoints
  monthlySpend: (threshold?: number) =>
    call<{ spend: MonthlySpendReport }>("GET", `/api/metrics/spend${threshold != null ? `?threshold=${threshold}` : ""}`).then((r) => r.spend),
  egressLog: (limit = 100) =>
    call<{ entries: EgressEntry[] }>("GET", `/api/egress?limit=${limit}`).then((r) => r.entries),
  runGc: (body?: { max_runs?: number; max_age_days?: number; prune_worktrees?: boolean }) =>
    call<{ gc: GcReport }>("POST", "/api/gc", body ?? {}).then((r) => r.gc),
  runners: () => call<{ runners: RemoteNode[] }>("GET", "/api/runners").then((r) => r.runners),
  upsertRunner: (body: { id?: string; name: string; url: string; labels?: string[]; max_parallel?: number; enabled?: boolean; token?: string }) =>
    call<{ runner: RemoteNode }>("POST", "/api/runners", body).then((r) => r.runner),
  removeRunner: (id: string) => call<{ removed: boolean }>("POST", `/api/runners/${enc(id)}/remove`, {}),
  probeRunner: (id: string) => call<{ health: NodeHealth }>("POST", `/api/runners/${enc(id)}/probe`, {}).then((r) => r.health),
  environments: () => call<{ environments: RemoteNode[] }>("GET", "/api/environments").then((r) => r.environments),
  upsertEnvironment: (body: { id?: string; name: string; url: string; labels?: string[]; max_parallel?: number; enabled?: boolean; token?: string }) =>
    call<{ environment: RemoteNode }>("POST", "/api/environments", body).then((r) => r.environment),
  removeEnvironment: (id: string) => call<{ removed: boolean }>("POST", `/api/environments/${enc(id)}/remove`, {}),
  mcpServers: () => call<{ servers: UserMcpServer[] }>("GET", "/api/mcp").then((r) => r.servers),
  upsertMcp: (body: { name: string; command: string; args?: string[]; env?: Record<string, string>; secret_env?: Record<string, string>; roles?: string[]; enabled?: boolean; require_approval?: boolean }) =>
    call<{ server: UserMcpServer }>("POST", "/api/mcp", body).then((r) => r.server),
  removeMcp: (name: string) => call<{ removed: boolean }>("POST", `/api/mcp/${enc(name)}/remove`, {}),
  notifications: () => call<{ notifications: NotificationConfig }>("GET", "/api/notifications").then((r) => r.notifications),
  updateNotifications: (cfg: NotificationConfig) =>
    call<{ notifications: NotificationConfig }>("POST", "/api/notifications", cfg).then((r) => r.notifications),
  testNotification: () => call<{ sent: boolean }>("POST", "/api/notifications/test", {}),
  schedules: () => call<{ schedules: ScheduledRun[] }>("GET", "/api/schedules").then((r) => r.schedules),
  upsertSchedule: (body: { id?: string; name: string; schedule: string; prompt: string; enabled?: boolean; auto_approve?: boolean; max_cost_usd?: number }) =>
    call<{ schedule: ScheduledRun }>("POST", "/api/schedules", body).then((r) => r.schedule),
  toggleSchedule: (id: string, enabled: boolean) =>
    call<{ schedule: ScheduledRun }>("POST", `/api/schedules/${enc(id)}/toggle`, { enabled }).then((r) => r.schedule),
  removeSchedule: (id: string) => call<{ removed: boolean }>("POST", `/api/schedules/${enc(id)}/remove`, {}),
  parseIssue: (url: string, fetch = false) =>
    call<IssueParseResult>("POST", "/api/issues/parse", { url, fetch }),
  attachments: (id: string) =>
    call<{ attachments: AttachmentMeta[] }>("GET", `/api/runs/${enc(id)}/attachments`).then((r) => r.attachments),
  addAttachment: (id: string, body: { name: string; mime?: string; data_base64: string }) =>
    call<{ attachment: AttachmentMeta }>("POST", `/api/runs/${enc(id)}/attachments`, body).then((r) => r.attachment),
  prStatus: (id: string, pr?: string) =>
    call<{ checks: CiCheck[] }>("GET", `/api/runs/${enc(id)}/pr/status${pr ? `?pr=${enc(pr)}` : ""}`).then((r) => r.checks),
  prSync: (id: string, body?: { pr?: string; checks?: CiCheck[]; comments?: { path: string; line?: number; author: string; body: string }[] }) =>
    call<{ sync: PrSyncReport }>("POST", `/api/runs/${enc(id)}/pr/sync`, body ?? {}).then((r) => r.sync),
  stratifiedScorecard: (difficulty?: string, language?: string) => {
    const params = new URLSearchParams();
    if (difficulty) params.set("difficulty", difficulty);
    if (language) params.set("language", language);
    const qs = params.toString();
    return call<{ slices: StratifiedSlice[]; auto_route?: ModelScore | null }>("GET", `/api/scorecard/stratified${qs ? `?${qs}` : ""}`);
  },
  evalSuite: () => call<{ tasks: EvalSuiteTask[] }>("GET", "/api/eval/suite").then((r) => r.tasks),

  // Phase 1 web access settings, cache management, and per-run web activity
  webAccess: () => call<WebAccessPayload>("GET", "/api/web-access"),
  updateWebAccess: (body: WebAccessUpdate) => call<WebAccessPayload>("POST", "/api/web-access", body),
  clearWebCache: () => call<{ removed: number; cache: WebCacheStats }>("POST", "/api/web-access/cache/clear", {}),
  runWebActivity: (id: string) =>
    call<{ items: WebActivityItem[] }>("GET", `/api/runs/${enc(id)}/web`).then((r) => r.items),
};

/**
 * Follow a run's log as it grows. The daemon sends every event after `after`, then one
 * `idle` with the phase when nothing is driving the run, and stops. A browser would
 * reconnect on its own, so the source is closed there; the caller opens a new one once
 * the run is moving again.
 */
export function follow(
  id: string,
  after: number,
  on: {
    event: (event: RunEvent, index: number) => void;
    agent?: (entry: AgentEntry) => void;
    idle: (phase: Phase) => void;
    lost: () => void;
  },
  agentsAfter = 0,
): () => void {
  const params = new URLSearchParams({ after: String(after), agents_after: String(agentsAfter) });
  const bearer = token();
  if (bearer) params.set("token", bearer);
  const source = new EventSource(`${server()}/api/runs/${enc(id)}/stream?${params}`);
  source.addEventListener("event", (message) => {
    const m = message as MessageEvent<string>;
    on.event(JSON.parse(m.data) as RunEvent, Number(m.lastEventId));
  });
  source.addEventListener("agent", (message) => {
    on.agent?.(JSON.parse((message as MessageEvent<string>).data) as AgentEntry);
  });
  source.addEventListener("idle", (message) => {
    source.close();
    on.idle((JSON.parse((message as MessageEvent<string>).data) as { phase: Phase }).phase);
  });
  source.onerror = () => {
    if (source.readyState === EventSource.CLOSED) on.lost();
  };
  return () => source.close();
}

/** Follow a setup job until it finishes. Each message is the whole job as it is now. */
export function followJob(id: string, on: (job: SetupJob) => void, lost: () => void): () => void {
  const params = new URLSearchParams();
  const bearer = token();
  if (bearer) params.set("token", bearer);
  const source = new EventSource(`${server()}/api/setup/jobs/${enc(id)}/stream?${params}`);
  source.addEventListener("job", (message) => {
    const job = JSON.parse((message as MessageEvent<string>).data) as SetupJob;
    on(job);
    if (job.state !== "running") source.close();
  });
  source.onerror = () => {
    if (source.readyState === EventSource.CLOSED) lost();
  };
  return () => source.close();
}

/** Follow a terminal's output from byte `from`. Bytes arrive decoded; `exit` ends it. */
export function followTerminal(
  id: string,
  from: number,
  on: { data: (bytes: Uint8Array, end: number) => void; exit: (code: number | null) => void; lost: () => void },
): () => void {
  const params = new URLSearchParams({ from: String(from) });
  const bearer = token();
  if (bearer) params.set("token", bearer);
  const source = new EventSource(`${server()}/api/terminals/${enc(id)}/stream?${params}`);
  source.addEventListener("data", (message) => {
    const { at, b64 } = JSON.parse((message as MessageEvent<string>).data) as { at: number; b64: string };
    const raw = atob(b64);
    const bytes = new Uint8Array(raw.length);
    for (let i = 0; i < raw.length; i++) bytes[i] = raw.charCodeAt(i);
    on.data(bytes, at + bytes.length);
  });
  source.addEventListener("exit", (message) => {
    source.close();
    on.exit((JSON.parse((message as MessageEvent<string>).data) as { code: number | null }).code);
  });
  source.onerror = () => {
    if (source.readyState === EventSource.CLOSED) on.lost();
  };
  return () => source.close();
}

/** Per-row throttled live command output stream (100ms frames, 16 KiB append then tail replace, P5). */
export function followCommandOutput(
  runId: string,
  toolId: string,
  on: { frame: (frame: CommandOutputFrame) => void; done?: () => void },
): () => void {
  const params = new URLSearchParams({ tool_id: toolId });
  const bearer = token();
  if (bearer) params.set("token", bearer);
  const source = new EventSource(`${server()}/api/runs/${enc(runId)}/output/stream?${params}`);
  source.addEventListener("output", (message) => {
    const frame = JSON.parse((message as MessageEvent<string>).data) as CommandOutputFrame;
    on.frame(frame);
  });
  source.addEventListener("done", () => {
    source.close();
    on.done?.();
  });
  source.onerror = () => {
    if (source.readyState === EventSource.CLOSED) on.done?.();
  };
  return () => source.close();
}

// Accounts. PLACEHOLDER: the sign-in service does not exist yet. The daemon's
// `.taskmaster/config.json` names it (`auth.issuer`) and `/api/health` passes that on as
// `sign_in`. The contract assumed here: this page sends the browser to
// `<sign_in>/authorize?redirect_uri=<this app>/signed-in`, and the service sends it back to
// `/signed-in#token=<jwt>`, a token signed with the secret the daemon shares (HS256, with
// `iss` set to the issuer, `sub` and `exp`). Change `signInUrl` when the service is built.

export function signInUrl(signIn: string): string {
  const back = `${window.location.origin}/signed-in`;
  return `${signIn.replace(/\/+$/, "")}/authorize?redirect_uri=${enc(back)}`;
}

/** The token a sign-in service handed back in the URL fragment, if any. */
export function tokenFromRedirect(): string | null {
  const hash = new URLSearchParams(window.location.hash.slice(1));
  return hash.get("token");
}
