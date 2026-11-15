//! The permission gate: the daemon's answer to "may this worker do that?".

use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Read,
    Edit,
    Execute,
    Other,
}

#[derive(Debug, Clone)]
pub struct PermissionRequest {
    pub tool: String,
    pub kind: ToolKind,
    /// Files the call touches, when the harness reports them.
    pub paths: Vec<PathBuf>,
    pub command: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}

pub trait PermissionGate: Send + Sync {
    fn decide(&self, request: &PermissionRequest) -> Decision;
}

/// Confines a worker to its worktree: file access inside `root` only, no shell, and only
/// the tools named in `allowed_tool_prefixes` (the Dictionary) beyond that.
#[derive(Debug, Clone)]
pub struct WorktreeGate {
    root: PathBuf,
    allowed_tool_prefixes: Vec<String>,
}

impl WorktreeGate {
    pub fn new(root: impl Into<PathBuf>, allowed_tool_prefixes: Vec<String>) -> Self {
        let root = root.into();
        let root = std::fs::canonicalize(&root).unwrap_or_else(|_| normalize(&root));
        WorktreeGate { root, allowed_tool_prefixes }
    }

    fn inside(&self, path: &Path) -> bool {
        let absolute = if path.is_absolute() { path.to_path_buf() } else { self.root.join(path) };
        // Resolve symlinks where the path exists, so a link cannot point out of the worktree.
        let resolved = std::fs::canonicalize(&absolute).unwrap_or_else(|_| normalize(&absolute));
        resolved.starts_with(&self.root)
    }
}

impl PermissionGate for WorktreeGate {
    fn decide(&self, request: &PermissionRequest) -> Decision {
        if self.allowed_tool_prefixes.iter().any(|p| request.tool.starts_with(p.as_str())) {
            return Decision::Allow;
        }
        match request.kind {
            ToolKind::Read | ToolKind::Edit
                if !request.paths.is_empty() && request.paths.iter().all(|p| self.inside(p)) =>
            {
                Decision::Allow
            }
            _ => Decision::Deny,
        }
    }
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
}
