//! OS-level confinement for harnesses that do not ask before touching files.
//!
//! Antigravity 1.5.9 sends permission requests for shell commands but reads and writes
//! files anywhere without asking, so the gate alone cannot confine it. The agent is run
//! under bubblewrap instead: the filesystem is read-only, `/tmp` and the home directory are
//! replaced by empty scratch mounts, and only the worktree and the harness's own state
//! directories are mounted writable.
//!
//! The same sandbox is what makes a worker's shell safe. A command the agent runs is its
//! child, so it is inside the same mounts: it can write the worktree, its own scratch and
//! cache directories (a [`ShellScope`](crate::ShellScope)), `/tmp` and an empty home, and
//! nothing else. Toolchains installed under the home directory are mounted read-only so
//! `poetry`, `uv`, `cargo` and the like still run.

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
    /// A sandbox whose only writable places, besides the worktree, are `state_dirs`
    /// (given relative to the home directory). Directories that do not exist are left out.
    pub fn with_state_dirs(state_dirs: &[&str]) -> anyhow::Result<Self> {
        let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
        let state_dirs = state_dirs.iter().map(|d| home.join(d)).filter(|d| d.exists()).collect();
        Ok(Sandbox { home, state_dirs })
    }

    pub fn antigravity() -> anyhow::Result<Self> {
        Self::with_state_dirs(&[".gemini", ".config/antigravity"])
    }

    pub fn grok() -> anyhow::Result<Self> {
        Self::with_state_dirs(&[".grok"])
    }

    pub fn opencode() -> anyhow::Result<Self> {
        Self::with_state_dirs(&[
            ".opencode",
            ".config/opencode",
            ".local/share/opencode",
            ".local/state/opencode",
            ".cache/opencode",
        ])
    }

    /// Arguments for `bwrap` that run `binary args...` confined to `worktree`.
    pub fn bwrap_args(
        &self,
        binary: &Path,
        args: &[String],
        worktree: &Path,
        read_only: bool,
    ) -> Vec<OsString> {
        self.bwrap_args_with(binary, args, worktree, read_only, None)
    }

    /// [`Sandbox::bwrap_args`], with a shell's scratch and cache directories writable, its
    /// environment set, and the toolchains under the home directory mounted read-only.
    pub fn bwrap_args_with(
        &self,
        binary: &Path,
        args: &[String],
        worktree: &Path,
        read_only: bool,
        shell: Option<&crate::ShellScope>,
    ) -> Vec<OsString> {
        let mut out: Vec<OsString> = Vec::new();
        let mut push = |parts: &[&dyn AsRef<std::ffi::OsStr>]| {
            out.extend(parts.iter().map(|p| p.as_ref().to_os_string()));
        };
        push(&[&"--ro-bind", &"/", &"/", &"--dev", &"/dev", &"--proc", &"/proc"]);
        // Hide everything a worker has no business reading: other worktrees, the user's files.
        push(&[&"--tmpfs", &"/tmp", &"--tmpfs", &self.home]);
        // Mounts below must come after the scratch mounts they sit inside.
        push(&[&"--ro-bind", &binary, &binary]);
        if let Some(shell) = shell.filter(|_| !read_only) {
            for dir in TOOL_DIRS.iter().map(|d| self.home.join(d)).filter(|d| d.exists()) {
                push(&[&"--ro-bind", &dir, &dir]);
            }
            for dir in &shell.writable {
                let _ = std::fs::create_dir_all(dir);
                push(&[&"--bind", dir, dir]);
            }
            for dir in &shell.readable {
                if dir.exists() {
                    push(&[&"--ro-bind", dir, dir]);
                }
            }
            for name in LEAKY_VARS {
                push(&[&"--unsetenv", &name]);
            }
            push(&[&"--setenv", &"PATH", &clean_path()]);
            for (key, value) in &shell.env {
                push(&[&"--setenv", key, value]);
            }
        }
        for dir in &self.state_dirs {
            push(&[&"--bind", dir, dir]);
        }
        let mount = if read_only { "--ro-bind" } else { "--bind" };
        push(&[&mount, &worktree, &worktree]);
        // Keep `.flow/` read-only inside writable worker sandboxes so only Taskmaster writes it.
        if !read_only {
            let flow_dir = worktree.join(".flow");
            if flow_dir.is_dir() {
                push(&[&"--ro-bind", &flow_dir, &flow_dir]);
            }
        }
        push(&[&"--die-with-parent", &"--chdir", &worktree, &"--", &binary]);
        out.extend(args.iter().map(OsString::from));
        out
    }
}

/// Toolchain directories under the home directory a shell may run programs from. They are
/// mounted read-only; nothing in them holds a credential.
pub const TOOL_DIRS: [&str; 12] = [
    ".local/bin",
    ".local/pipx",
    ".local/share/pipx",
    ".local/share/uv",
    ".cargo/bin",
    ".rustup",
    ".nvm",
    ".bun/bin",
    ".deno/bin",
    ".pyenv",
    ".volta",
    ".local/share/pnpm",
];

/// Variables that tie a process to someone else's Python environment.
pub const LEAKY_VARS: [&str; 7] = ["VIRTUAL_ENV", "PYTHONPATH", "PYTHONHOME", "POETRY_ACTIVE", "PIPENV_ACTIVE", "UV_PROJECT_ENVIRONMENT", "__PYVENV_LAUNCHER__"];

/// `PATH` without the directories of a virtualenv or a Conda environment the daemon was
/// started from, so `python` and `pytest` are the system's, not that environment's.
pub fn clean_path() -> OsString {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let skip: Vec<PathBuf> = ["VIRTUAL_ENV", "CONDA_PREFIX"].iter().filter_map(std::env::var_os).map(PathBuf::from).collect();
    clean_path_of(&path, &skip)
}

/// `path` without the directories under `skip` or inside a virtualenv or Conda environment.
pub fn clean_path_of(path: &std::ffi::OsStr, skip: &[PathBuf]) -> OsString {
    let kept: Vec<PathBuf> = std::env::split_paths(path)
        .filter(|dir| !skip.iter().any(|s| !s.as_os_str().is_empty() && dir.starts_with(s)))
        .filter(|dir| !dir.parent().is_some_and(|parent| parent.join("pyvenv.cfg").is_file() || parent.join("conda-meta").is_dir()))
        .collect();
    std::env::join_paths(kept).unwrap_or_else(|_| path.to_os_string())
}

/// Whether bubblewrap can make a sandbox here; some containers forbid user namespaces.
/// Checked once.
pub fn bwrap_usable() -> bool {
    static USABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *USABLE.get_or_init(|| {
        if cfg!(not(target_os = "linux")) {
            return false;
        }
        std::process::Command::new("bwrap")
            .args(["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp", "--die-with-parent", "true"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// Find `name` on `PATH` and resolve symlinks, so the real file can be mounted in the sandbox.
pub fn resolve_binary(name: &str) -> anyhow::Result<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 {
        return std::fs::canonicalize(candidate).with_context(|| format!("`{name}` not found"));
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    // Installers put a harness in its own directory and add that to the shell profile.
    // A daemon started from somewhere else does not have that profile, so look there too.
    for dir in std::env::split_paths(&path).chain(crate::setup::install_dirs()) {
        for file in program_names(name) {
            if let Ok(found) = std::fs::canonicalize(dir.join(&file))
                && found.is_file() {
                    return Ok(found);
                }
        }
    }
    bail!("`{name}` was not found on PATH")
}

/// The file names a program may have. Windows adds an extension (`claude.exe`, or
/// `codex.cmd` for an npm install), so each of `PATHEXT` is tried there.
pub fn program_names(name: &str) -> Vec<String> {
    if !cfg!(windows) || Path::new(name).extension().is_some() {
        return vec![name.to_string()];
    }
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    exts.split(';').filter(|e| !e.is_empty()).map(|e| format!("{name}{}", e.to_ascii_lowercase())).collect()
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
        let args =
            sandbox.bwrap_args(Path::new("/home/u/.local/bin/agy"), &["acp".into()], worktree, false);
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
        let args = sandbox.bwrap_args(Path::new("/bin/agy"), &[], Path::new("/w"), false);
        assert_eq!(&args[..3], ["--ro-bind", "/", "/"]);
    }

    #[test]
    fn a_shell_gets_its_scratch_writable_and_no_leaked_environment() {
        let sandbox = Sandbox { home: "/home/u".into(), state_dirs: vec![] };
        let scope = crate::ShellScope {
            writable: vec![std::env::temp_dir().join(format!("tm-shell-scope-{}", std::process::id()))],
            readable: vec![],
            env: vec![("TASKMASTER_SCRATCH".into(), "/s".into())],
        };
        let args = sandbox.bwrap_args_with(Path::new("/bin/grok"), &[], Path::new("/w"), false, Some(&scope));
        let args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        let scratch = scope.writable[0].display().to_string();
        let at = |needle: &[&str]| args.windows(needle.len()).position(|w| w == needle);
        assert!(at(&["--bind", &scratch, &scratch]).unwrap() > at(&["--tmpfs", "/tmp"]).unwrap(), "scratch is mounted over /tmp");
        assert!(at(&["--unsetenv", "VIRTUAL_ENV"]).is_some());
        assert!(at(&["--setenv", "TASKMASTER_SCRATCH", "/s"]).is_some());
        assert_eq!(args[..3], ["--ro-bind", "/", "/"], "everything else stays read-only");
        // A reviewer never gets the shell's mounts, even if one is passed.
        let ro = sandbox.bwrap_args_with(Path::new("/bin/grok"), &[], Path::new("/w"), true, Some(&scope));
        assert!(!ro.iter().any(|a| a.to_string_lossy() == scratch));
        let _ = std::fs::remove_dir_all(&scope.writable[0]);
    }

    /// The sandbox really holds: a command inside it can write the worktree and its scratch,
    /// and nothing else. Skipped where bubblewrap cannot run.
    #[test]
    fn a_shell_in_the_sandbox_writes_only_where_it_may() {
        if !bwrap_usable() {
            eprintln!("bubblewrap is not usable here; skipping");
            return;
        }
        let base = std::env::temp_dir().join(format!("tm-sandbox-real-{}", std::process::id()));
        let (work, scratch, outside) = (base.join("work"), base.join("scratch"), base.join("outside"));
        for dir in [&work, &scratch, &outside] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/root".into()));
        let sandbox = Sandbox { home, state_dirs: vec![] };
        let scope = crate::ShellScope { writable: vec![scratch.clone()], readable: vec![], env: vec![] };
        let script = format!(
            "echo a > {w}/ok; echo b > {s}/ok; echo c > {o}/no 2>/dev/null; echo d > /etc/tm-no 2>/dev/null; true",
            w = work.display(), s = scratch.display(), o = outside.display()
        );
        let sh = std::fs::canonicalize("/bin/sh").unwrap();
        let args = sandbox.bwrap_args_with(&sh, &["-c".into(), script], &work, false, Some(&scope));
        let status = std::process::Command::new("bwrap").args(&args).status().unwrap();
        assert!(status.success());
        assert!(work.join("ok").is_file(), "the worktree is writable");
        assert!(scratch.join("ok").is_file(), "the scratch directory is writable");
        assert!(!outside.join("no").exists(), "a directory outside is not");
        assert!(!Path::new("/etc/tm-no").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_venv_on_path_is_left_out() {
        let base = std::env::temp_dir().join(format!("tm-clean-path-{}", std::process::id()));
        std::fs::create_dir_all(base.join("venv/bin")).unwrap();
        std::fs::write(base.join("venv/pyvenv.cfg"), "home = /usr/bin\n").unwrap();
        let joined = std::env::join_paths([base.join("venv/bin"), PathBuf::from("/opt/conda/bin"), PathBuf::from("/usr/bin")]).unwrap();
        let clean = clean_path_of(&joined, &[PathBuf::from("/opt/conda")]);
        let dirs: Vec<PathBuf> = std::env::split_paths(&clean).collect();
        assert_eq!(dirs, vec![PathBuf::from("/usr/bin")]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_reviewer_gets_its_worktree_read_only() {
        let sandbox = Sandbox { home: "/home/u".into(), state_dirs: vec![] };
        let args = sandbox.bwrap_args(Path::new("/bin/agy"), &[], Path::new("/w"), true);
        let args: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert!(args.windows(3).any(|w| w == ["--ro-bind", "/w", "/w"]));
        assert!(!args.windows(2).any(|w| w == ["--bind", "/w"]));
    }

    #[test]
    fn flow_dir_and_shared_readable_venv_are_mounted_read_only_in_worker_sandbox() {
        let base = std::env::temp_dir().join(format!("tm-sandbox-flow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let work = base.join("work");
        let flow = work.join(".flow");
        let venv = base.join("shared-venv");
        std::fs::create_dir_all(&flow).unwrap();
        std::fs::create_dir_all(&venv).unwrap();

        let sandbox = Sandbox { home: "/home/u".into(), state_dirs: vec![] };
        let scope = crate::ShellScope {
            writable: vec![],
            readable: vec![venv.clone()],
            env: vec![],
        };
        let args = sandbox.bwrap_args_with(Path::new("/bin/sh"), &[], &work, false, Some(&scope));
        let args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        let flow_s = flow.display().to_string();
        let work_s = work.display().to_string();
        let venv_s = venv.display().to_string();
        let pos = |needle: &[&str]| args.windows(needle.len()).position(|w| w == needle);
        let work_pos = pos(&["--bind", &work_s, &work_s]).expect("worktree bound writable");
        let flow_pos = pos(&["--ro-bind", &flow_s, &flow_s]).expect(".flow bound read-only");
        assert!(flow_pos > work_pos, ".flow ro-bind must come after worktree bind");
        assert!(pos(&["--ro-bind", &venv_s, &venv_s]).is_some(), "shared readable venv must be ro-bound");

        let _ = std::fs::remove_dir_all(&base);
    }
}
