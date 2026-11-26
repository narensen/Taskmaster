use serde::{Deserialize, Serialize};

pub type PageId = String;
pub type DictKey = String;

/// Dictionary keys and page ids become directory names, so they stay one path segment.
pub fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 64 && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DictTier {
    Local,
    Commons,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Schema,
    Routes,
    ComponentApi,
    Pattern,
    Hint,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntryStats {
    pub reuses: u64,
    pub pass_rate: f32,
    /// Successful uses of this Commons entry.
    #[serde(default)]
    pub passes: u64,
    /// Uses that failed their page.
    #[serde(default)]
    pub fails: u64,
}

/// One immutable Dictionary entry. A change is published as a new entry with a new hash.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DictionaryEntry {
    pub key: DictKey,
    pub tier: DictTier,
    pub kind: EntryKind,
    /// One line, shown in the index every worker sees.
    pub summary: String,
    pub content: String,
    /// Content address.
    pub hash: String,
    /// Stack the entry was verified against, e.g. `next@15,react@19`.
    pub stack: String,
    /// Commons entries only.
    pub stats: Option<EntryStats>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContractFile {
    pub path: String,
    pub content: String,
    /// The phase of the sketch this contract belongs to (0-indexed).
    /// Defaults to 0 (first phase) for single-phase plans or phase-1 contracts.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub phase: usize,
}

impl ContractFile {
    /// A contract that is an executable acceptance test rather than an interface.
    pub fn is_test(&self) -> bool {
        is_test_path(&self.path)
    }
}

/// A test file by its runner's naming (`cart.test.ts`, `cart.spec.js`, `test_cart.py`,
/// `cart_test.py`, `cart_test.go`) or a source file under a `tests`, `__tests__`, `test`,
/// or `spec` directory. Fixture files (`conftest.py`, `__init__.py`) are not tests.
pub fn is_test_path(path: &str) -> bool {
    let path = path.trim_start_matches("./");
    let name = path.rsplit('/').next().unwrap_or(path);
    if matches!(name, "conftest.py" | "__init__.py" | "mod.rs") || !name.contains('.') {
        return false;
    }
    name.contains(".test.")
        || name.contains(".spec.")
        || (name.starts_with("test_") && name.ends_with(".py"))
        || name.ends_with("_test.py")
        || name.ends_with("_test.go")
        || path.split('/').rev().skip(1).any(|dir| matches!(dir, "tests" | "__tests__" | "test" | "spec"))
}

/// What a plan builds. Only a web project has routes, a design system and browser checks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectKind {
    /// Pages a browser shows. Plans written before kinds existed are all this.
    #[default]
    Web,
    Library,
    Cli,
    Server,
    Migration,
}

impl ProjectKind {
    pub const ALL: [ProjectKind; 5] = [ProjectKind::Web, ProjectKind::Library, ProjectKind::Cli, ProjectKind::Server, ProjectKind::Migration];

    pub fn id(self) -> &'static str {
        match self {
            ProjectKind::Web => "web",
            ProjectKind::Library => "library",
            ProjectKind::Cli => "cli",
            ProjectKind::Server => "server",
            ProjectKind::Migration => "migration",
        }
    }

    /// The kind a planner named, read loosely: `lib`, `api`, `upgrade` and the like.
    pub fn parse(text: &str) -> Option<ProjectKind> {
        Some(match text.trim().to_ascii_lowercase().as_str() {
            "web" | "website" | "site" | "webapp" | "web-app" | "app" | "frontend" | "ui" => ProjectKind::Web,
            "library" | "lib" | "package" | "module" | "sdk" => ProjectKind::Library,
            "cli" | "tool" | "command" | "script" => ProjectKind::Cli,
            "server" | "service" | "api" | "daemon" | "backend" => ProjectKind::Server,
            "migration" | "upgrade" | "port" | "refactor" | "dependency" | "dependencies" => ProjectKind::Migration,
            _ => return None,
        })
    }
}

/// One phase of the rough sketch a run starts from (PROJECT.md). Only the first phase is
/// planned in detail up front; each later one is planned once the phases before it have
/// locked their interfaces.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PhaseSketch {
    /// A short slug, such as `storage`.
    pub id: String,
    pub goal: String,
    /// Modules or files this phase delivers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delivers: Vec<String>,
    /// Its public interfaces, as signatures only.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub interfaces: String,
    /// The command that runs this phase's tests.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub test: String,
    /// `results` when this phase needs what the one before it built, not only its locked
    /// interfaces; it is then planned after that phase passes instead of while it builds.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub needs: String,
}

impl PhaseSketch {
    /// Whether this phase can be planned while the one before it is still building.
    pub fn overlaps(&self) -> bool {
        !self.needs.trim().eq_ignore_ascii_case("results")
    }
}

/// One unit of a plan: a page of a web app, or a module of anything else. One agent builds
/// it, or several when it is split into parts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PagePlan {
    pub id: PageId,
    /// Where a browser opens it. Empty for a unit nothing shows in a browser.
    #[serde(default)]
    pub route: String,
    /// Set when a unit of a non-web plan is shown in a browser after all, or when a unit of
    /// a web plan is not (`false`). `None` follows the plan's kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<bool>,
    /// The only files this page's worker may change, e.g. `app/dashboard/**`.
    #[serde(default)]
    pub owned_glob: String,
    #[serde(default)]
    pub cites: Vec<DictKey>,
    #[serde(default)]
    pub brief: String,
    /// Self-contained context for this unit's worker: architecture notes, key symbols,
    /// integration details, and conventions it needs without re-exploring the whole repo.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub context: String,
    /// How the page's agent knows it is finished: what must exist and work.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub done: String,
    /// Optional split of this page into parallel work units with disjoint files. Empty
    /// means one worker owns the whole page. Planners write it as `parts` or `workers`.
    #[serde(default, alias = "parts", alias = "workers", skip_serializing_if = "Vec::is_empty")]
    pub units: Vec<WorkUnit>,
    /// Installed skills the planner attached: names from the skills store. The page's
    /// agents find each one's SKILL.md in their worktree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// The phase of the sketch this unit belongs to, from 0.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub phase: usize,
    /// Optional IDs of other units/pages that must finish before this unit starts,
    /// enabling fine-grained parallel DAG execution without waiting on a whole phase.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// Optional complexity hint (`easy`, `normal`, `hard`) set by the planner to skip
    /// unnecessary escalation on hard units and reduce reasoning effort on easy ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub difficulty: Option<String>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// One worker's share of a page: a disjoint set of files and what to build in them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkUnit {
    pub id: String,
    /// Files this unit may change. Braces list several: `{app/index.html,app/styles.css}`.
    pub owned_glob: String,
    pub brief: String,
    /// Self-contained context for this worker (contracts, integration details, key types).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub context: String,
    /// How this unit's agent knows it is finished: what must exist and work.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub done: String,
    /// Optional upstream unit IDs this worker depends on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
}

/// The most units a page is split into.
pub const MAX_UNITS: usize = 16;

impl PagePlan {
    /// True when the planner marked this unit `hard` (complex state, concurrency, or deep integration).
    pub fn is_hard(&self) -> bool {
        self.difficulty
            .as_deref()
            .is_some_and(|d| matches!(d.trim().to_ascii_lowercase().as_str(), "hard" | "high" | "complex"))
    }

    /// Reasoning effort hint derived from `difficulty`: `low` for easy, `high` for hard. A
    /// normal unit keeps the harness's own default.
    pub fn effort_hint(&self) -> Option<&'static str> {
        match self.difficulty.as_deref()?.trim().to_ascii_lowercase().as_str() {
            "easy" | "low" | "simple" => Some("low"),
            "hard" | "high" | "complex" => Some("high"),
            _ => None,
        }
    }
    /// The units to run. The declared split when it is safe (2 to [`MAX_UNITS`] units, unique
    /// ids, no overlapping files); otherwise a single unit owning the whole page.
    pub fn work_units(&self) -> Vec<WorkUnit> {
        if units_disjoint(&self.units) {
            return self.units.clone();
        }
        vec![WorkUnit {
            id: self.id.clone(),
            owned_glob: self.owned_glob.clone(),
            brief: self.brief.clone(),
            context: self.context.clone(),
            done: self.done.clone(),
            depends_on: self.depends_on.clone(),
        }]
    }
}

/// Whether a declared split can be trusted: 2..=MAX_UNITS units, valid unique ids, relative
/// globs, and no two units whose globs could match the same file.
pub fn units_disjoint(units: &[WorkUnit]) -> bool {
    if units.len() < 2 || units.len() > MAX_UNITS {
        return false;
    }
    let mut ids = std::collections::BTreeSet::new();
    if !units.iter().all(|u| valid_key(&u.id) && ids.insert(u.id.as_str()) && !u.owned_glob.trim().is_empty() && !u.brief.trim().is_empty()) {
        return false;
    }
    let alts: Vec<Vec<String>> = units.iter().map(|u| glob_alternatives(&u.owned_glob)).collect();
    if alts.iter().flatten().any(|g| g.starts_with('/') || g.split('/').any(|c| c == "..")) {
        return false;
    }
    for (i, a) in alts.iter().enumerate() {
        for b in &alts[i + 1..] {
            if a.iter().any(|x| b.iter().any(|y| globs_may_overlap(x, y))) {
                return false;
            }
        }
    }
    true
}

/// Split one top-level `{a,b}` group into its alternatives; any other glob is its own.
fn glob_alternatives(glob: &str) -> Vec<String> {
    let glob = glob.trim();
    if let Some(inner) = glob.strip_prefix('{').and_then(|g| g.strip_suffix('}'))
        && !inner.contains(['{', '}'])
    {
        return inner.split(',').map(|g| g.trim().to_string()).collect();
    }
    vec![glob.to_string()]
}

/// True when two unit `owned_glob` patterns may match the same file, expanding top-level
/// `{a,b}` brace groups and checking literal/wildcard overlap. Empty globs never overlap.
pub fn owned_globs_overlap(a: &str, b: &str) -> bool {
    let norm = |g: &str| -> Vec<String> {
        glob_alternatives(g)
            .into_iter()
            .map(|s| s.trim().trim_start_matches("./").to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };
    let alts_a = norm(a);
    let alts_b = norm(b);
    if alts_a.is_empty() || alts_b.is_empty() {
        return false;
    }
    alts_a
        .iter()
        .any(|x| alts_b.iter().any(|y| globs_may_overlap(x, y)))
}

/// Conservative: true unless the two globs are provably disjoint by literal prefix, or by
/// being two different literal paths.
fn globs_may_overlap(a: &str, b: &str) -> bool {
    let meta = |g: &str| g.find(['*', '?', '[', '{']);
    match (meta(a), meta(b)) {
        (None, None) => a == b,
        (None, Some(_)) => !literal_cannot_match(a, b),
        (Some(_), None) => !literal_cannot_match(b, a),
        (Some(i), Some(j)) => {
            let (pa, pb) = (&a[..i], &b[..j]);
            pa.starts_with(pb) || pb.starts_with(pa)
        }
    }
}

/// A literal path against a glob: only a prefix mismatch is treated as proof.
fn literal_cannot_match(path: &str, glob: &str) -> bool {
    let at = glob.find(['*', '?', '[', '{']).unwrap_or(glob.len());
    let prefix = &glob[..at];
    !(path.starts_with(prefix) || prefix.starts_with(path))
}

/// One agent a plan will start: who it is, what it builds, the files it owns, and how it
/// knows it is finished.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentPlan {
    /// The id it runs under: `page:<page>`, or `page:<page>/<unit>` when the page is split.
    pub id: String,
    pub page: PageId,
    pub owned_glob: String,
    pub task: String,
    pub done: String,
}

/// What an agent is told when the plan gave it no done criteria.
pub const DEFAULT_DONE: &str = "every file you own is written and does its part of the brief";

impl Plan {
    /// Whether a unit is shown in a browser: every unit of a web plan unless it says
    /// `"ui": false`, and a unit of any other plan only when it says `"ui": true`.
    pub fn is_ui(&self, page: &PagePlan) -> bool {
        page.ui.unwrap_or(self.kind == ProjectKind::Web)
    }

    /// The id an agent of this unit runs under: `page:<id>` for a page a browser shows, and
    /// `unit:<id>` for anything else, with `/<part>` for one part of a split unit.
    pub fn agent_id(&self, page: &PagePlan, part: Option<&str>) -> String {
        let prefix = if self.is_ui(page) { "page" } else { "unit" };
        match part {
            Some(part) => format!("{prefix}:{}/{part}", page.id),
            None => format!("{prefix}:{}", page.id),
        }
    }

    /// Units a browser opens, with their routes.
    pub fn ui_routes(&self) -> Vec<(PageId, String)> {
        self.pages.iter().filter(|p| self.is_ui(p) && !p.route.trim().is_empty()).map(|p| (p.id.clone(), p.route.clone())).collect()
    }

    /// Whether anything in this plan is shown in a browser.
    pub fn has_ui(&self) -> bool {
        self.pages.iter().any(|p| self.is_ui(p))
    }

    /// Every build agent the plan starts, in order.
    pub fn agents(&self) -> Vec<AgentPlan> {
        let mut out = Vec::new();
        for page in &self.pages {
            let units = page.work_units();
            let split = units.len() > 1;
            for unit in units {
                let id = self.agent_id(page, split.then_some(unit.id.as_str()));
                let done = if unit.done.trim().is_empty() { DEFAULT_DONE.to_string() } else { unit.done.trim().to_string() };
                out.push(AgentPlan { id, page: page.id.clone(), owned_glob: unit.owned_glob, task: unit.brief.trim().to_string(), done });
            }
        }
        out
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    /// Fresh tokens: input not served from a cache, and output.
    pub tokens: u64,
    pub usd: f64,
    /// Input the provider is expected to serve from its cache, counted apart.
    #[serde(default)]
    pub cache_read: u64,
    /// Whether `usd` is a price at all. Without past prices or a price table it is not.
    #[serde(default)]
    pub priced: bool,
}

/// One decision produced by the planner's research phase (Item 1.4), carrying its source
/// URL and target version so workers inherit only compact decisions in `PROJECT.md`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResearchNote {
    pub decision: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// What is being built. Plans recorded before kinds existed read as web.
    #[serde(default)]
    pub kind: ProjectKind,
    /// One-paragraph overview of the plan and how work is divided across workers.
    #[serde(default, alias = "overview", skip_serializing_if = "String::is_empty")]
    pub summary: String,
    /// Real source files that workers import; the compiler enforces them.
    pub contracts: Vec<ContractFile>,
    /// The units. Planners write the key as `units` or `workers`; records keep `pages`.
    #[serde(alias = "units", alias = "workers")]
    pub pages: Vec<PagePlan>,
    /// The rough sketch, when the run is built a phase at a time. Empty for a small job,
    /// which is one phase planned whole.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<PhaseSketch>,
    pub estimate: Estimate,
    /// The planner's design spec, shared by every agent: palette, type, spacing, component
    /// look, empty state, responsive layout and accessibility rules.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub design: String,
    /// Research decisions from the planner's research phase, written into `PROJECT.md`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub research: Vec<ResearchNote>,
}

/// A worker's objection to a Dictionary entry, raised instead of editing around it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Flag {
    pub page: PageId,
    pub key: DictKey,
    pub issue: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlagResolution {
    /// The entry stands; the worker must conform to it.
    Conform,
    /// The Taskmaster published a successor entry.
    Amended,
}

/// One connection to review: a Dictionary entry and the pages that consume it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewScope {
    pub key: DictKey,
    pub consumers: Vec<PageId>,
}

/// A reviewer's claim that something is broken. Reviewers never edit; a finding is routed
/// to the page's owning worker as a repair.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub page: PageId,
    /// The connection the reviewer was looking at, if any.
    pub key: Option<DictKey>,
    pub file: String,
    pub line: u32,
    /// The concrete failure: what input or state leads to what wrong result.
    pub failure: String,
}

impl Finding {
    /// A finding without a location and a concrete failure is dropped: acting on a vague
    /// one costs a repair round for nothing.
    pub fn is_actionable(&self) -> bool {
        !self.file.trim().is_empty() && self.line > 0 && self.failure.trim().len() >= 10
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    ContractsLocked,
    /// The master repaired a shared file the run wrote, after the project's checks failed on it.
    SharedRepaired,
    PageBuilt,
    PageRepaired,
    EntryAmended,
    ReviewFinding,
    VerifyFailed,
    /// A verify gate finding was waived after a no-op repair produced an identical failure.
    GateWarning,
}

/// One line of the change log: what changed, where, and who changed it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeEntry {
    /// RFC 3339 timestamp, supplied by the executor.
    pub at: String,
    pub kind: ChangeKind,
    pub page: Option<PageId>,
    pub commit: Option<String>,
    /// Model that made the change.
    pub model: Option<String>,
    pub files: Vec<String>,
    /// Dictionary keys the change touches or depends on.
    pub keys: Vec<DictKey>,
    pub summary: String,
}

/// True when `path` is a non-empty relative path with only normal components (no `/`, `..`, or `.`
/// traversal). Globs such as `app/home/**` pass.
pub fn safe_rel(path: &str) -> bool {
    let path = std::path::Path::new(path);
    !path.is_absolute()
        && !path.as_os_str().is_empty()
        && path.components().all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// True when `path` refers to `.flow` itself or any file inside `.flow/` (Taskmaster's managed
/// code knowledge graph).
pub fn is_flow_path(path: &str) -> bool {
    let norm = path.replace('\\', "/");
    let trimmed = norm.trim_start_matches("./").trim_end_matches('/');
    trimmed == ".flow" || trimmed.starts_with(".flow/")
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    fn unit(id: &str, glob: &str) -> WorkUnit {
        WorkUnit { id: id.into(), owned_glob: glob.into(), brief: "x".into(), ..Default::default() }
    }

    fn page(units: Vec<WorkUnit>) -> PagePlan {
        PagePlan {
            id: "todo".into(),
            route: "/".into(),
            ui: None,
            owned_glob: "{app/**,README.md}".into(),
            cites: vec![],
            brief: "b".into(),
            context: String::new(),
            done: String::new(),
            units,
            skills: vec![],
            phase: 0,
            ..Default::default()
        }
    }

    #[test]
    fn contract_tests_are_told_apart_from_interfaces() {
        for test in ["tests/contract/cart.ts", "app/cart.test.ts", "web/cart.spec.tsx", "test_cart.py", "pkg/cart_test.py", "cart_test.go", "crate/tests/api.rs", "src/__tests__/cart.js"] {
            assert!(is_test_path(test), "{test}");
        }
        for interface in ["contracts/types.ts", "conftest.py", "tests/conftest.py", "tests/__init__.py", "src/testing.ts", "latest.py", "tests/README"] {
            assert!(!is_test_path(interface), "{interface}");
        }
    }

    #[test]
    fn a_plan_lists_one_agent_per_unit() {
        let mut units = vec![unit("ui", "app/index.html"), unit("js", "app/main.js")];
        units[1].done = "adding a todo shows it".into();
        let plan = Plan { pages: vec![page(units), page(vec![])], ..Default::default() };
        let agents = plan.agents();
        let ids: Vec<&str> = agents.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["page:todo/ui", "page:todo/js", "page:todo"]);
        assert_eq!(agents.len(), 3);
        assert_eq!(agents[0].done, DEFAULT_DONE);
        assert_eq!(agents[1].done, "adding a todo shows it");
        assert_eq!(agents[1].owned_glob, "app/main.js");
    }

    #[test]
    fn disjoint_units_are_kept() {
        let units = vec![unit("ui", "{app/index.html,app/styles.css}"), unit("js", "app/main.js"), unit("docs", "README.md")];
        assert_eq!(page(units.clone()).work_units(), units);
    }

    #[test]
    fn overlapping_units_fall_back_to_one() {
        for units in [
            vec![unit("a", "app/**"), unit("b", "app/main.js")],
            vec![unit("a", "app/**"), unit("b", "app/x/**")],
            vec![unit("a", "app/main.js"), unit("b", "{app/main.js,README.md}")],
        ] {
            let got = page(units).work_units();
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].owned_glob, "{app/**,README.md}");
        }
    }

    #[test]
    fn bad_splits_fall_back_to_one() {
        assert_eq!(page(vec![]).work_units().len(), 1);
        assert_eq!(page(vec![unit("a", "app/a.js")]).work_units().len(), 1);
        assert_eq!(page(vec![unit("a", "a.js"), unit("a", "b.js")]).work_units().len(), 1);
        assert_eq!(page(vec![unit("a", "a.js"), unit("b", "../b.js")]).work_units().len(), 1);
        let too_many: Vec<_> = (0..=MAX_UNITS).map(|i| unit(&format!("u{i}"), &format!("f{i}.js"))).collect();
        assert_eq!(page(too_many).work_units().len(), 1);
    }

    #[test]
    fn safe_rel_and_is_flow_path_handle_traversal_and_separators() {
        assert!(safe_rel("app/home/**"));
        assert!(safe_rel("contracts/types.ts"));
        assert!(!safe_rel(""));
        assert!(!safe_rel("/etc/passwd"));
        assert!(!safe_rel("../outside.py"));
        assert!(!safe_rel("app/../outside.py"));
        assert!(!safe_rel("./app/main.py"));

        assert!(is_flow_path(".flow"));
        assert!(is_flow_path(".flow/meta.json"));
        assert!(is_flow_path("./.flow/files/a.py.json"));
        assert!(is_flow_path(".flow\\index.sqlite"));
        assert!(!is_flow_path(".flow2/meta.json"));
        assert!(!is_flow_path("src/.flow/meta.json"));
    }
}
