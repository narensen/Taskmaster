//! The permission gate: the daemon's answer to "may this worker do that?".

use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Read,
    Edit,
    Execute,
    /// A fetch from the network.
    Fetch,
    /// A call to the Dictionary's MCP server. The adapter recognises it, because each
    /// harness names MCP tools differently.
    Dictionary,
    /// Taskmaster's browser tool. Allowed only on the run's own preview origin.
    Browser,
    Other,
}

#[derive(Debug, Clone)]
pub struct PermissionRequest {
    pub tool: String,
    pub kind: ToolKind,
    /// Files the call touches, when the harness reports them.
    pub paths: Vec<PathBuf>,
    pub command: Option<String>,
    /// The address a browser call opens, when it names one.
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}

#[async_trait::async_trait]
pub trait PermissionGate: Send + Sync {
    fn decide(&self, request: &PermissionRequest) -> Decision;

    /// Like `decide`, but a gate may wait for a person to answer. Adapters call this one.
    async fn decide_async(&self, request: &PermissionRequest) -> Decision {
        self.decide(request)
    }
}

/// Confines a worker to its worktree: file access inside `root` only, a shell only when the
/// agent runs inside the sandbox, fetches only from the hosts it is given, and only the tools
/// named in `allowed_tool_prefixes` (the Dictionary) beyond that.
#[derive(Debug, Clone)]
pub struct WorktreeGate {
    root: PathBuf,
    allowed_tool_prefixes: Vec<String>,
    /// Extra read-only locations, e.g. the harness's own state directory.
    readable: Vec<PathBuf>,
    read_only: bool,
    /// The one origin the browser tool may open (`http://127.0.0.1:<port>`). `None` refuses
    /// every browser call.
    browser: Option<String>,
    /// Shell commands are allowed: the agent runs inside the sandbox, so they are confined.
    shell: bool,
    /// Hosts a fetch may reach, such as the package indexes.
    fetch_hosts: Vec<String>,
    /// Allow the harness's built-in web search tool when provider is set to `native`.
    native_search: bool,
    /// Full-access per-run permission mode: bypass confinement refusals for non-read-only tasks.
    full_access: bool,
}

impl WorktreeGate {
    pub fn new(root: impl Into<PathBuf>, allowed_tool_prefixes: Vec<String>) -> Self {
        let root = root.into();
        let root = std::fs::canonicalize(&root).unwrap_or_else(|_| normalize(&root));
        WorktreeGate {
            root,
            allowed_tool_prefixes,
            readable: Vec::new(),
            read_only: false,
            browser: None,
            shell: false,
            fetch_hosts: Vec::new(),
            native_search: false,
            full_access: false,
        }
    }

    /// Allow full access (shell, network, and edits) for non-read-only tasks when explicitly selected.
    pub fn with_full_access(mut self) -> Self {
        self.full_access = true;
        self
    }

    /// Allow shell commands. Give this only to an agent that runs inside the sandbox.
    pub fn with_shell(mut self) -> Self {
        self.shell = true;
        self
    }

    /// Allow fetches from these hosts and their subdomains, and from nowhere else.
    pub fn with_fetch_hosts(mut self, hosts: &[&str]) -> Self {
        self.fetch_hosts = hosts.iter().map(|h| h.to_ascii_lowercase()).collect();
        self
    }

    /// Allow the harness's native web search tool (used when Web access provider is `native`).
    pub fn with_native_search(mut self) -> Self {
        self.native_search = true;
        self
    }

    pub fn allows_shell(&self) -> bool {
        (self.shell || self.full_access) && !self.read_only
    }

    /// Refuse every edit, for reviewers and verifiers.
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// Allow reads (never writes) under `dir`.
    pub fn with_readable(mut self, dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        self.readable.push(std::fs::canonicalize(&dir).unwrap_or_else(|_| normalize(&dir)));
        self
    }

    /// Allow the browser tool, on `origin` only.
    pub fn with_browser(mut self, origin: impl Into<String>) -> Self {
        self.browser = Some(origin.into().trim_end_matches('/').to_string());
        self
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Why a call the gate refuses is still worth putting to a person: it runs a shell
    /// command, fetches from the network, or writes outside the worktree. `None` for
    /// anything else, such as a read outside the worktree or any edit by a read-only
    /// agent, which stay refused.
    pub fn risk(&self, request: &PermissionRequest) -> Option<&'static str> {
        if self.native_search && is_native_search_tool(request) {
            return None;
        }
        if self.read_only || self.full_access {
            return None;
        }
        match request.kind {
            ToolKind::Execute if self.allows_shell() => None,
            ToolKind::Execute => Some("runs a shell command"),
            ToolKind::Fetch if self.fetch_allowed(request) => None,
            ToolKind::Fetch => Some("fetches from the network"),
            ToolKind::Edit if !request.paths.is_empty() && !request.paths.iter().all(|p| self.inside(p)) => {
                Some("writes outside the worktree")
            }
            _ => None,
        }
    }

    fn fetch_allowed(&self, request: &PermissionRequest) -> bool {
        let Some(host) = request.url.as_deref().and_then(host_of) else { return false };
        self.fetch_hosts.iter().any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")))
    }

    /// The path relative to the worktree root, or `None` when it lies outside it.
    pub fn relative(&self, path: &Path) -> Option<PathBuf> {
        self.resolve(path).strip_prefix(&self.root).ok().map(Path::to_path_buf)
    }

    fn inside(&self, path: &Path) -> bool {
        self.resolve(path).starts_with(&self.root)
    }

    fn readable(&self, path: &Path) -> bool {
        let resolved = self.resolve(path);
        resolved.starts_with(&self.root) || self.readable.iter().any(|r| resolved.starts_with(r))
    }

    fn resolve(&self, path: &Path) -> PathBuf {
        let absolute = if path.is_absolute() { path.to_path_buf() } else { self.root.join(path) };
        // Resolve symlinks where the path exists, so a link cannot point out of the worktree.
        std::fs::canonicalize(&absolute).unwrap_or_else(|_| normalize(&absolute))
    }
}

impl PermissionGate for WorktreeGate {
    fn decide(&self, request: &PermissionRequest) -> Decision {
        // Before the prefixes: the browser tool is served beside the Dictionary, but it
        // may only open the run's own preview.
        if request.kind == ToolKind::Browser {
            let allowed = match (&self.browser, &request.url) {
                (Some(_), None) => true,
                (Some(origin), Some(url)) => same_origin(origin, url),
                (None, _) => false,
            };
            return if allowed { Decision::Allow } else { Decision::Deny };
        }
        if self.full_access && !self.read_only {
            return Decision::Allow;
        }
        if self.native_search && is_native_search_tool(request) {
            return Decision::Allow;
        }
        if self.allowed_tool_prefixes.iter().any(|p| request.tool.starts_with(p.as_str())) {
            return Decision::Allow;
        }
        match request.kind {
            ToolKind::Dictionary => Decision::Allow,
            ToolKind::Read
                if !request.paths.is_empty() && request.paths.iter().all(|p| self.readable(p)) =>
            {
                Decision::Allow
            }
            ToolKind::Edit
                if !self.read_only
                    && !request.paths.is_empty()
                    && request.paths.iter().all(|p| self.inside(p)) =>
            {
                Decision::Allow
            }
            ToolKind::Execute if self.allows_shell() => Decision::Allow,
            ToolKind::Fetch if !self.read_only && self.fetch_allowed(request) => Decision::Allow,
            _ => Decision::Deny,
        }
    }
}

/// True when `request` invokes a harness's built-in web search tool (never edits or shell).
fn is_native_search_tool(request: &PermissionRequest) -> bool {
    if matches!(request.kind, ToolKind::Edit | ToolKind::Execute | ToolKind::Browser) {
        return false;
    }
    let norm = request.tool.trim().to_ascii_lowercase().replace(['-', ' '], "_");
    matches!(
        norm.as_str(),
        "web_search"
            | "websearch"
            | "search_web"
            | "google_search"
            | "bing_search"
            | "brave_search"
            | "internet_search"
    )
}

/// Whether `url` is on `origin` (`scheme://host:port`), or is a path on it.
pub fn same_origin(origin: &str, url: &str) -> bool {
    if url.starts_with('/') && !url.starts_with("//") {
        return true;
    }
    let origin = origin.trim_end_matches('/');
    match url.strip_prefix(origin) {
        Some(rest) => rest.is_empty() || rest.starts_with(['/', '?', '#']),
        None => false,
    }
}

/// The host of an `http` or `https` address, lower-cased, without a port or credentials.
/// `None` for anything else.
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    // `user:pass@host` names `host`; a lookalike such as `pypi.org@evil.test` is `evil.test`.
    let host = authority.rsplit('@').next()?;
    let host = if host.starts_with('[') { host.split(']').next()?.trim_start_matches('[') } else { host.split(':').next()? };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Resolve `.` and `..` without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(tool: &str, kind: ToolKind, paths: &[&str]) -> PermissionRequest {
        PermissionRequest {
            tool: tool.into(),
            kind,
            paths: paths.iter().map(PathBuf::from).collect(),
            command: None,
            url: None,
        }
    }

    fn gate() -> WorktreeGate {
        WorktreeGate::new("/nonexistent/tm/work-a", vec!["mcp_dict_".into()])
    }

    #[test]
    fn allows_files_inside_the_worktree() {
        let g = gate();
        let inside = request("write_file", ToolKind::Edit, &["/nonexistent/tm/work-a/app/page.tsx"]);
        assert_eq!(g.decide(&inside), Decision::Allow);
        let relative = request("view_file", ToolKind::Read, &["app/page.tsx"]);
        assert_eq!(g.decide(&relative), Decision::Allow);
    }

    #[test]
    fn denies_files_outside_the_worktree() {
        let g = gate();
        let sibling = request("view_file", ToolKind::Read, &["/nonexistent/tm/work-b/greeting.ts"]);
        assert_eq!(g.decide(&sibling), Decision::Deny);
        let escape = request("write_file", ToolKind::Edit, &["../work-b/x.ts"]);
        assert_eq!(g.decide(&escape), Decision::Deny);
        // A sibling whose name merely starts with the worktree's name is still outside.
        let lookalike = request("view_file", ToolKind::Read, &["/nonexistent/tm/work-a2/x.ts"]);
        assert_eq!(g.decide(&lookalike), Decision::Deny);
    }

    #[test]
    fn denies_shell_and_unknown_tools_but_allows_the_dictionary() {
        let g = gate();
        assert_eq!(g.decide(&request("run_command", ToolKind::Execute, &[])), Decision::Deny);
        assert_eq!(g.decide(&request("browser", ToolKind::Other, &[])), Decision::Deny);
        assert_eq!(g.decide(&request("view_file", ToolKind::Read, &[])), Decision::Deny);
        assert_eq!(g.decide(&request("mcp_dict_cite", ToolKind::Other, &[])), Decision::Allow);
        assert_eq!(g.decide(&request("dict__cite", ToolKind::Dictionary, &[])), Decision::Allow);
        let strict = WorktreeGate::new("/nonexistent/tm/work-a", vec![]);
        assert_eq!(strict.decide(&request("other__tool", ToolKind::Other, &[])), Decision::Deny);
    }

    #[test]
    fn a_sandboxed_worker_runs_commands_and_fetches_only_from_package_indexes() {
        let fetch = |url: &str| PermissionRequest { tool: "web_fetch".into(), kind: ToolKind::Fetch, paths: vec![], command: None, url: Some(url.into()) };
        let g = gate().with_shell().with_fetch_hosts(&crate::PACKAGE_INDEXES);
        assert_eq!(g.decide(&request("run_command", ToolKind::Execute, &[])), Decision::Allow);
        assert!(g.risk(&request("run_command", ToolKind::Execute, &[])).is_none(), "nothing to ask a person");
        assert_eq!(g.decide(&fetch("https://pypi.org/pypi/pydantic/json")), Decision::Allow);
        assert_eq!(g.decide(&fetch("https://files.pythonhosted.org/packages/x.whl")), Decision::Allow);
        assert_eq!(g.decide(&fetch("https://example.com/")), Decision::Deny);
        assert_eq!(g.decide(&fetch("https://pypi.org@evil.test/")), Decision::Deny);
        assert_eq!(g.decide(&fetch("https://pypi.org.evil.test/")), Decision::Deny);
        assert_eq!(g.decide(&fetch("file:///etc/passwd")), Decision::Deny);
        // Without the sandbox there is no shell, and a reviewer never gets one.
        assert_eq!(gate().decide(&request("run_command", ToolKind::Execute, &[])), Decision::Deny);
        assert_eq!(gate().with_shell().read_only().decide(&request("run_command", ToolKind::Execute, &[])), Decision::Deny);
        assert_eq!(gate().with_fetch_hosts(&crate::PACKAGE_INDEXES).read_only().decide(&fetch("https://pypi.org/")), Decision::Deny);
        // Writes outside the worktree stay refused with a shell.
        assert_eq!(g.decide(&request("write_file", ToolKind::Edit, &["/etc/hosts"])), Decision::Deny);
    }

    #[test]
    fn a_hosts_name_is_read_without_port_or_credentials() {
        assert_eq!(host_of("https://PyPI.org:443/simple").as_deref(), Some("pypi.org"));
        assert_eq!(host_of("http://u:p@registry.npmjs.org/x").as_deref(), Some("registry.npmjs.org"));
        assert_eq!(host_of("ftp://pypi.org/"), None);
        assert_eq!(host_of("pypi.org"), None);
    }

    #[test]
    fn a_read_only_gate_refuses_edits_inside_the_worktree() {
        let g = gate().read_only();
        let edit = request("write_file", ToolKind::Edit, &["/nonexistent/tm/work-a/app/page.tsx"]);
        assert_eq!(g.decide(&edit), Decision::Deny);
        let read = request("view_file", ToolKind::Read, &["/nonexistent/tm/work-a/app/page.tsx"]);
        assert_eq!(g.decide(&read), Decision::Allow);
    }

    #[test]
    fn extra_readable_dirs_are_read_only() {
        let g = gate().with_readable("/nonexistent/state");
        let read = request("view_file", ToolKind::Read, &["/nonexistent/state/brain/x.md"]);
        assert_eq!(g.decide(&read), Decision::Allow);
        let write = request("write_file", ToolKind::Edit, &["/nonexistent/state/brain/x.md"]);
        assert_eq!(g.decide(&write), Decision::Deny);
    }

    #[test]
    fn the_browser_opens_only_the_preview_origin() {
        let browse = |url: Option<&str>| PermissionRequest {
            tool: "mcp__dictionary__browser_open".into(),
            kind: ToolKind::Browser,
            paths: vec![],
            command: None,
            url: url.map(str::to_string),
        };
        // Without a preview, no browser at all, even through the Dictionary's prefix.
        assert_eq!(gate().decide(&browse(Some("/"))), Decision::Deny);
        let g = gate().with_browser("http://127.0.0.1:4321/");
        assert_eq!(g.decide(&browse(None)), Decision::Allow);
        assert_eq!(g.decide(&browse(Some("/cart?x=1"))), Decision::Allow);
        assert_eq!(g.decide(&browse(Some("http://127.0.0.1:4321/cart"))), Decision::Allow);
        assert_eq!(g.decide(&browse(Some("http://127.0.0.1:4321"))), Decision::Allow);
        assert_eq!(g.decide(&browse(Some("http://127.0.0.1:43210/"))), Decision::Deny);
        assert_eq!(g.decide(&browse(Some("http://127.0.0.1:8737/api/runs"))), Decision::Deny);
        assert_eq!(g.decide(&browse(Some("https://example.com/"))), Decision::Deny);
        assert_eq!(g.decide(&browse(Some("//example.com/"))), Decision::Deny);
        assert_eq!(g.decide(&browse(Some("http://127.0.0.1:4321@evil.test/"))), Decision::Deny);
        assert!(g.risk(&browse(Some("https://example.com/"))).is_none(), "never put to a person");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_cannot_lead_out_of_the_worktree() {
        let base = std::env::temp_dir().join(format!("tm-gate-{}", std::process::id()));
        let (root, outside) = (base.join("work"), base.join("secret"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("key.txt"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        let g = WorktreeGate::new(&root, vec![]);
        let via_link = root.join("link/key.txt");
        let req = request("view_file", ToolKind::Read, &[via_link.to_str().unwrap()]);
        assert_eq!(g.decide(&req), Decision::Deny);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn native_search_passthrough_allows_search_only() {
        let search = PermissionRequest {
            tool: "WebSearch".into(),
            kind: ToolKind::Fetch,
            paths: vec![],
            command: None,
            url: None,
        };
        let fetch = PermissionRequest {
            tool: "WebFetch".into(),
            kind: ToolKind::Fetch,
            paths: vec![],
            command: None,
            url: Some("https://example.com".into()),
        };
        // Denied by default.
        assert_eq!(gate().read_only().decide(&search), Decision::Deny);
        // Allowed when opted in, including for read-only roles (planner/reviewer).
        let g = gate().read_only().with_native_search();
        assert_eq!(g.decide(&search), Decision::Allow);
        assert!(g.risk(&search).is_none());
        // Does not widen WebFetch or shell.
        assert_eq!(g.decide(&fetch), Decision::Deny);
        assert_eq!(g.decide(&request("run_command", ToolKind::Execute, &[])), Decision::Deny);
    }
}
