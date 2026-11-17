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
  cache_read: number;
  cost_usd: number;
  unpriced_calls: number;
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
  units?: { id: string; owned_glob: string; brief: string }[];
}

export interface Plan {
  contracts: ContractFile[];
  pages: PagePlan[];
  estimate: Estimate;
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
  worktree?: string;
  branch?: string | null;
  call?: number;
}

export interface AgentEntry {
  at: string;
  agent: AgentTag;
  kind: "start" | "text" | "thought" | "tool_call" | "permission" | "end" | "command" | "output" | "preview" | "approval_requested" | "approval_resolved";
  text?: string;
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
  dictionary: string;
  commons: string;
  commons_remote: boolean;
  config: string | null;
  config_default: string;
  store: string;
  roles: Roles;
  models?: Models;
  version: string;
  approvals?: { mode: ApprovalMode; timeout_secs: number };
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
  path: string;
}

export interface GhStatus {
  installed: boolean;
  account: string | null;
  error?: string;
}

export const DEFAULT_ROLES: Roles = {
  worker: "grok",
  mid: "grok",
  master: "grok",
  reviewer: "grok",
  verifier: "grok",
};

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

async function call<T>(method: "GET" | "POST", path: string, body?: unknown): Promise<T> {
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
  setRoleDefaults: (body: { roles: Roles; models: Models }) => call<DaemonSettings>("POST", "/api/settings", body),
  setApprovals: (body: { approvals?: ApprovalMode; approval_timeout_secs?: number }) => call("POST", "/api/settings", body),
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
  skills: () => call<{ skills: Skill[] }>("GET", "/api/skills").then((r) => r.skills),
  gh: () => call<GhStatus>("GET", "/api/gh"),
  ghPr: (n: number) => call<{ number: number; title: string; url: string; state: string }>("GET", `/api/gh/pr/${n}`),

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
