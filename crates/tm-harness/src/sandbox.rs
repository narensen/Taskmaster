//! OS-level confinement for harnesses that do not ask before touching files.
//!
//! Antigravity 1.5.9 sends permission requests for shell commands but reads and writes
//! files anywhere without asking, so the gate alone cannot confine it. The agent is run
//! under bubblewrap instead: the filesystem is read-only, `/tmp` and the home directory are
//! replaced by empty scratch mounts, and only the worktree and the harness's own state
//! directories are mounted writable.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

#[derive(Debug, Clone)]
pub struct Sandbox {
    pub home: PathBuf,
    /// Directories the harness needs for its login and session state.
    pub state_dirs: Vec<PathBuf>,
}

impl Sandbox {
    pub fn antigravity() -> anyhow::Result<Self> {
        let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
        let state_dirs = vec![home.join(".gemini"), home.join(".config/antigravity")];
        Ok(Sandbox { home, state_dirs })
    }

    /// Arguments for `bwrap` that run `binary args...` confined to `worktree`.
    pub fn bwrap_args(&self, binary: &Path, args: &[String], worktree: &Path) -> Vec<OsString> {
        let mut out: Vec<OsString> = Vec::new();
        let mut push = |parts: &[&dyn AsRef<std::ffi::OsStr>]| {
            out.extend(parts.iter().map(|p| p.as_ref().to_os_string()));
        };
        push(&[&"--ro-bind", &"/", &"/", &"--dev", &"/dev", &"--proc", &"/proc"]);
        // Hide everything a worker has no business reading: other worktrees, the user's files.
        push(&[&"--tmpfs", &"/tmp", &"--tmpfs", &self.home]);
        // Mounts below must come after the scratch mounts they sit inside.
        push(&[&"--ro-bind", &binary, &binary]);
        for dir in &self.state_dirs {
            push(&[&"--bind", dir, dir]);
        }
        push(&[&"--bind", &worktree, &worktree]);
        push(&[&"--die-with-parent", &"--chdir", &worktree, &"--", &binary]);
        out.extend(args.iter().map(OsString::from));
        out
    }
}

/// Find `name` on `PATH` and resolve symlinks, so the real file can be mounted in the sandbox.
pub fn resolve_binary(name: &str) -> anyhow::Result<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 {
        return std::fs::canonicalize(candidate).with_context(|| format!("`{name}` not found"));
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        if let Ok(found) = std::fs::canonicalize(dir.join(name)) {
            if found.is_file() {
                return Ok(found);
            }
        }
    }
    bail!("`{name}` was not found on PATH")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_is_mounted_after_the_scratch_mounts() {
        let sandbox = Sandbox {
            home: "/home/u".into(),
            state_dirs: vec!["/home/u/.gemini".into()],
        };
        let worktree = Path::new("/home/u/.taskmaster/worktrees/a");
        let args = sandbox.bwrap_args(Path::new("/home/u/.local/bin/agy"), &["acp".into()], worktree);
        let args: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        let position = |needle: &[&str]| {
            args.windows(needle.len()).position(|w| w == needle).expect("argument present")
        };
        let home_scratch = position(&["--tmpfs", "/home/u"]);
        let worktree_bind = position(&["--bind", worktree.to_str().unwrap()]);
        // A worktree under the home directory would be hidden if it were mounted first.
        assert!(home_scratch < worktree_bind);
        assert!(position(&["--tmpfs", "/tmp"]) < worktree_bind);
        assert!(home_scratch < position(&["--bind", "/home/u/.gemini"]));
        assert_eq!(&args[args.len() - 3..], ["--", "/home/u/.local/bin/agy", "acp"]);
    }

    #[test]
    fn the_root_filesystem_is_read_only() {
        let sandbox = Sandbox { home: "/home/u".into(), state_dirs: vec![] };
        let args = sandbox.bwrap_args(Path::new("/bin/agy"), &[], Path::new("/w"));
        assert_eq!(&args[..3], ["--ro-bind", "/", "/"]);
    }
}
