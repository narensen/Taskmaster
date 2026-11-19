//! One log for this daemon.
//!
//! Lines go to `$XDG_STATE_HOME/taskmaster/daemon.log`, or
//! `~/.local/state/taskmaster/daemon.log` when that variable is unset. The Settings
//! page reads the same file. A terminal the daemon was started from still shows
//! what it prints there; this file is the copy that stays after the terminal is gone.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

static WRITE: Mutex<()> = Mutex::new(());

pub fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))?;
    Some(base.join("taskmaster/daemon.log"))
}

pub fn note(line: &str) {
    let Some(path) = path() else { return };
    let _guard = WRITE.lock().unwrap();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else { return };
    let _ = writeln!(file, "{} {line}", crate::time::rfc3339_now());
}

/// The last `max_lines` lines, and the path they were read from.
pub fn tail(max_lines: usize) -> (Option<String>, Vec<String>) {
    let Some(path) = path() else { return (None, Vec::new()) };
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if lines.len() > max_lines {
        lines = lines.split_off(lines.len() - max_lines);
    }
    (Some(path.display().to_string()), lines)
}
