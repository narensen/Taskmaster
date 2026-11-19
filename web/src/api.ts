// The daemon's HTTP API, typed after the Rust structs that produce it
// (crates/tm-daemon/src/api.rs and the tm-core types it serialises).

export type Phase = "planning" | "awaiting_approval" | "building" | "reviewing" | "verifying" | "done" | "failed";
export type PageState = "queued" | "building" | "flagged" | "passed" | "failed";
export type Tier = "worker" | "mid" | "master";
export type RoleName = "worker" | "mid" | "master" | "reviewer" | "verifier";

export interface Estimate {
  tokens: number;
  usd: number;
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
  /** How the page's agent knows it is finished. */
  done?: string;
  units?: { id: string; owned_glob: string; brief: string; done?: string }[];
  /** Skills from the store the planner attached; the page's agents read each SKILL.md. */
  skills?: string[];
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
    const split = units.length >= 2 && units.length <= 4;
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

export interface Plan {
  contracts: ContractFile[];
  pages: PagePlan[];
  estimate: Estimate;
  /** The planner's design spec every agent follows, when anything is shown in a browser. */
  design?: string;
}

export type Roles = Record<RoleName, string>;
export type Models = Partial<Record<RoleName, string | null>>;

export interface RunSummary {
  id: string;
  phase: Phase;
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
  pages: PageView[];
  estimate: Estimate | null;
  spent: Totals;
  review_rounds: number;
  amendments: number;
  /** Risky tool calls waiting for a person. */
  pending_approvals?: number;
}

export interface RunLimit {
  provider: string;
  role: string;
  kind: "quota" | "rate_limit" | "auth";
  reset?: string | null;
  raw: string;
}

export interface RunDetail extends RunSummary {
  plan: Plan | null;
  prompt: string | null;
  roles: Roles;
  models: Models;
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

export type WorkerOutcome = "passed" | { check_failed: { errors: string } } | { flagged: Flag };

// Serde's externally tagged enum: a bare string for a unit variant, else one key.
export type RunEvent =
  | "plan_approved"
  | "plan_rejected"
  | "cancelled"
  | "follow_up"
  | { plan_ready: Plan }
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

export interface NewRun {
  prompt?: string;
  id?: string;
  yes?: boolean;
  roles?: Partial<Roles>;
  models?: Models;
  parallel?: number;
  timeout_secs?: number;
  template?: string;
}

export interface NewProject {
  name: string;
  prompt?: string;
  yes?: boolean;
  roles?: Partial<Roles>;
  models?: Models;
  parallel?: number;
  timeout_secs?: number;
  template?: string;
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
  kind: "start" | "text" | "thought" | "tool_call" | "permission" | "end" | "command" | "output" | "preview" | "approval_requested" | "approval_resolved" | "stalled" | "released";
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
}

export interface Note {
  id: string;
  to: string;
  text: string;
  at: string;
  delivered: boolean;
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
  telemetry?: { enabled: boolean };
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

/** Call another daemon with its own address and token (for gallery cards of other daemons). */
export async function callAt<T>(base: string, bearer: string | null, path: string): Promise<T> {
  const response = await fetch(base.replace(/\/+$/, "") + path, {
    headers: bearer ? { authorization: `Bearer ${bearer}` } : {},
  });
  if (!response.ok) throw new ApiError(response.status, `${response.status} ${response.statusText}`);
  return (await response.json()) as T;
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
    throw new ApiError(0, `cannot reach the daemon${server() ? ` at ${server()}` : ""}`);
  }
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

export function searchWeb(q: string, endpoint: string) {
  const params = new URLSearchParams({ q });
  if (endpoint) params.set("endpoint", endpoint);
  return call<{ endpoint: string; results: SearchHit[] }>("GET", `/api/search?${params}`);
}

export const api = {
  health: () => call<Health>("GET", "/api/health"),
  pair: (code: string) => call<{ token: string }>("POST", "/api/pair", { code }),

  runs: () => call<{ runs: RunSummary[] }>("GET", "/api/runs").then((r) => r.runs),
  run: (id: string) => call<RunDetail>("GET", `/api/runs/${enc(id)}`),
  createRun: (body: NewRun) => call<{ id: string }>("POST", "/api/runs", body),
  createProject: (body: NewProject) => call<{ project: string; id?: string }>("POST", "/api/projects", body),
  events: (id: string, after = 0) =>
    call<{ next: number; events: RunEvent[] }>("GET", `/api/runs/${enc(id)}/events?after=${after}`),
  ledger: (id: string) => call<{ totals: Totals; entries: LedgerEntry[] }>("GET", `/api/runs/${enc(id)}/ledger`),
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
  cancel: (id: string) => call("POST", `/api/runs/${enc(id)}/cancel`, {}),

  providers: (harness?: string) =>
    call<{ providers: Provider[] }>("GET", `/api/providers${harness ? `?harness=${enc(harness)}` : ""}`).then(
      (r) => r.providers,
    ),
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

  agents: (id: string, after = 0) =>
    call<{ next: number; entries: AgentEntry[] }>("GET", `/api/runs/${enc(id)}/agents?after=${after}`),
  notes: (id: string) => call<{ notes: Note[] }>("GET", `/api/runs/${enc(id)}/notes`).then((r) => r.notes),
  addNote: (id: string, to: string, text: string) => call<Note>("POST", `/api/runs/${enc(id)}/notes`, { to, text }),
  editNote: (id: string, note: string, text?: string) =>
    call<{ notes: Note[] }>("POST", `/api/runs/${enc(id)}/notes/${enc(note)}`, text ? { text } : {}),
  terminal: (id: string, command: string) =>
    call<{ ok: boolean; code: number | null; output: string }>("POST", `/api/runs/${enc(id)}/terminal`, { command }),
  diff: (id: string, scope: "run" | "page", page?: string) =>
    call<{ scope: string; base: string; branch: string; diff: string }>(
      "GET",
      `/api/runs/${enc(id)}/diff?scope=${scope}${page ? `&page=${enc(page)}` : ""}`,
    ),
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
  setTelemetry: (enabled: boolean) => call<DaemonSettings>("POST", "/api/settings", { telemetry: enabled }),
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
